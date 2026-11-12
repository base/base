# base-history-store

Shared chain history store (archive) proof of concept. Nodes keep recent history locally. A
writer copies finalized history from the nodes' `archive_` export endpoint into an
S3-compatible bucket as immutable segments of reth static files. It indexes them in `SlateDB`
in the same bucket and publishes progress through one compare-and-swap (CAS) object,
`catalog.json`. Nodes read old history straight from the bucket. There is no history service.
The node-side RPC wiring is in `base-history-rpc`, and the writer process is
`bin/archive-writer`.

## Layout

The layout sits under an optional prefix:

| Object | Contents |
| --- | --- |
| `catalog.json` | `{generation, chain_id, genesis_hash, format: 1, N: [{from_block, blocks_per_segment}], H, E, dictionaries, index: "idx/", retired, writer_epoch}`. The only object that is overwritten, and only by CAS. |
| `seg/<k>/<epoch>/{headers,transactions,receipts,transaction_senders,account_changesets,storage_changesets}.{conf,off,dat}` | reth `NippyJar` static files for blocks `[kN, (k+1)N-1]`, uploaded by the writer holding `epoch`. |
| `seg/<k>/<epoch>/block_bodies.idx` | `N` × 16 bytes: `first_tx_num` u64 BE, then `tx_count` u64 BE. |
| `idx/` | `SlateDB` holding the `t‖tx_hash → (block u64 BE, index u32 BE)`, `b‖block_hash → block u64 BE` and `h‖hashed_key‖segment u64 BE → roaring bitmap` entries, plus `m‖segment u64 BE → JSON manifest {segment, epoch, files: {name: {sha256, size}}}` and the watermark `w → last indexed block u64 BE`. |

`hashed_key` is a 1-byte kind followed by a hash:
- Kind `0x00`, then `keccak(address)`, for accounts.
- Kind `0x01`, then `keccak(address)‖keccak(slot)`, for storage.

This follows reth's hashed-state convention.

## Modules

| Module | TDD | What it does |
| --- | --- | --- |
| `catalog` | §2.4, §7 | `Catalog` model and `CatalogStore`. `init` writes with `If-None-Match`, and `update` is a CAS on the `ETag`. `H` and `E` never regress. |
| `segment` | §2 | `SegmentBuilder` writes the six static files with reth's own `StaticFileProviderRW` in a temp dir, then reads them back. The builds are byte-deterministic. |
| `verify` | §4 | `ChunkVerifier` checks each chunk before it is built: the parent link, the header chain, the transactions root, the receipts root and sender recovery. |
| `index` | §2.3, §4 | `IndexWriter` puts each segment's entries, manifest and watermark in one atomic batch with one flush; opening it fences older `SlateDB` writers. `IndexReader` ignores entries above `H` and reports its watermark. |
| `source` | §3, §10 | `RpcChainSource` is the `archive_` client: `with_jwt` signs every request with reth's `AuthClientLayer` for a node's authenticated RPC port, `new` is unauthenticated. `SourcePool` rotates nodes and skips dead, bad or busy (`4008`) ones; when every node is only busy it returns `SourceBusy` and the service backs off instead of counting a stall. |
| `objects` | §4 | Segment uploads go to the writer's epoch directory with `If-None-Match: *`; `AlreadyExists` with identical bytes counts as success. Downloads read the epoch the manifest names. Listing and deletion cover every epoch of a segment. |
| `writer` | §4, §7 | `ArchiveWriter::step` = `prepare` (fetch → verify → confirm → build → upload → index) then `publish` (owned CAS). It halts on a finalized hash change and warns when the archive lags finalized by more than `2N`. Chunks are clamped to the smallest advertised `maxRangeBlocks` and halved on a typed too-large rejection, down to one block (`RangeTooLarge`). |
| `gc` | §4 | `OrphanGc` lists only `seg/<k>` for `k` in `H/N+1 ..= H/N+16` and deletes objects older than the minimum age (default 1h) that sit under an epoch older than its own and that no `m‖k` references. |
| `pruner` | §6 | `Pruner::retire` advances `E` by owned CAS. `sweep` tombstones, deletes and drops retired segments after the grace period (default 24h). Both cap work at `max_segments_per_pass` (default 4). |
| `service` | §4, §6 | `ArchiveService` drives the writer loop and runs maintenance (retire, sweep, orphan GC) on an interval, all under the writer's epoch. |
| `router` | §5 | `ArchiveSnapshot` resolves `H` and `E` once per request. `route` and `split` sort blocks into served, `HistoryPruned` and `Unavailable`. |
| `cache` | §5 | `SegmentCache` keeps a memory LRU of open segments and an optional disk LRU. Disk entries are namespaced by archive identity, filled single-flight into a `fill-*` directory and renamed into place, and kept within the byte budget; a segment larger than the budget, or one that cannot fit beside leased entries and other fills' reservations, is served from a temp dir. Every fill is checked against the segment manifest. |
| `reader` | §2.2, §5 | `ArchiveReader` serves headers, blocks, receipts, hash lookups, bloom-filtered logs, and account and storage history. It checks hashes, roots and recovered senders before returning data, so a mismatch gives `CorruptSegment` and never wrong data. Segment reads and their checks run on the blocking pool, and each block's transaction-root, sender and receipt-root checks run once per segment load (`SegmentVerification`). `logs` returns each hit block with its logs and stops with `TooManyLogs` past the caller's cap. The catalog snapshot is reused for `snapshot_ttl` (default 2 s). |
| `store` | §5 | `ArchiveStore::from_url` accepts `s3://bucket/prefix` (plus an optional endpoint for `MinIO`), `file://dir` and `memory://prefix`. `from_url_with(StoreOptions::reader())` gives nodes a 3 s connect timeout and at most 3 retries within 15 s, instead of `object_store`'s 10 retries over 3 minutes. |

## Consistency and safety

- **Index freshness.** A reader's `SlateDB` view trails the writer by up to one manifest poll, so it can see a catalog `H` before the index entries for `H`. Each segment batch also writes the watermark `w`. `SlateDB` applies a `WriteBatch` atomically (`slatedb-0.18.1` `src/batch.rs:23`, `Db::write` at `src/db.rs:1650`, test `test_mixed_batch_publishes_atomically` at `src/db.rs:11990`). So a view with `w ≥ H` holds every entry up to `H`. Readers return `Unavailable` (the node then answers locally, or 4444) instead of "unknown hash" or `Unchanged` while `w < H`. History lookups check this before the scan, because a missing middle segment would skip a change.
- **Integrity.** Header hashes and the transaction and receipt roots cover headers, transactions and receipts. Senders are recovered from signatures the first time each block is read from a loaded segment; the segment's files cannot change while it is loaded, so later reads reuse that check. Changesets and every other file are covered by the per-file SHA-256 manifest `m‖k`, written in the same batch as the segment's index entries. The cache checks the manifest, including its block range, on every download and every disk-cache load. A missing manifest is `CorruptSegment`.
- **Ownership.** One protocol, tied to `SlateDB`: opening a writer opens the `SlateDB` index first, which fences older `SlateDB` writers and bumps the manifest's `writer_epoch`, then sets the catalog's `writer_epoch` to that same number by CAS, only if it is greater than the stored one (`CatalogStore::claim_at`); otherwise the open fails with `Fenced`. Two instances opening concurrently in either order leave exactly one that can publish. Publish, retire, sweep and GC changes are CAS updates conditional on the catalog still holding the instance's epoch, so a writer paused anywhere (even after its index flush) and then superseded gets `Fenced` and never moves `H`.
- **Epoch-scoped objects and GC.** Object stores have no conditional DELETE, so a GC that passed its ownership and `H` checks can still land a delete after a newer owner took over. Every writer therefore uploads under its own `seg/<k>/<epoch>/`, the manifest `m‖k` records that epoch, and readers download from it. GC reads `H` from a catalog version it re-reads after its own claim and that holds its epoch, before each delete, and deletes only objects under epochs strictly older than its own that `m‖k` does not reference. A stale GC's last delete can only hit an older epoch's directory, which no newer owner reuses; a restarted writer re-uploads at most one partial segment.
- **Superseded uploads.** Before uploading segment `k` the writer deletes every epoch directory under `seg/<k>/` older than its own. It owns the catalog and `k > H/N`, so no published manifest references them. This removes a crashed owner's partial upload even though GC never revisits `k` once it is published. Known `PoC` gap: a superseded writer that is still paused mid-upload can add objects to its own old epoch directory after this delete. Those objects are unreachable and are not reclaimed when the segment is already published and the retention sweep has run (or retention is off). Each such writer can strand at most one segment's objects, at most once, because it is fenced at its next catalog check. Closing this fully needs a conditional delete or a bucket-wide orphan scan.
- **Retirement.** `retire` only advances `E` and records the segment in `retired`. Readers treat every block `< E` as `HistoryPruned` whatever `retired` holds; readers on an older snapshot still find the entries and objects until the grace period ends. `sweep` then: (a) if `m‖k` still exists, tombstones the segment's recomputed keys and `m‖k` in one atomic batch; (b) deletes `seg/<k>/*` across every epoch; (c) CASes `k` out of `retired`. A missing `m‖k` proves (a) ran, because tombstones are flushed before any delete, so a crash at any step converges on the next sweep. `retired` only holds segments within their grace period.
- **Orphan window.** A writer only uploads segment `H_read/N + 1` for the `H` it read, and `H` never decreases. A stale writer's upload of a segment `≤ H/N` lands in its own epoch directory, which no manifest references. Orphans therefore sit at `H/N + 1`; GC lists `H/N+1 ..= H/N+16` as margin and never lists published segments.
- **Index readiness.** Number-based reads check that the reader's index view covers the segment (`w ≥ (k+1)N-1`) before loading it, so a view that has not seen `m‖k` yet answers `Unavailable`, never `CorruptSegment`. Hash lookups read coverage before the lookup; when the view did not cover `H` and the entry is absent, they wait for coverage and look up once more.
- **Cache lifetime and budget.** reth opens a segment's `NippyJar` files lazily, on first access, so a `LoadedSegment` holds a lease on its disk entry and eviction skips leased entries. A fill reserves its manifest's total size before downloading; the reservation counts against the budget for every concurrent fill until the entry is published. The fill lock entry, the reservation and the `fill-*` directory are released by drop guards, so a cancelled read leaves nothing behind. Opening the cache removes `fill-*` directories a crashed process left (never valid, since publication is an atomic rename) and evicts persisted entries least recently used first until they fit the current budget, deleting any entry larger than the whole budget, all before the first lease.
- **Standalone maintenance.** `archive-writer gc` and `prune` open a full writer and claim the archive, which fences a running `archive-writer run`. Use them only while the service is stopped; in normal operation `run` does maintenance itself.

## State history (§2.2)

`account_history` and `storage_history` return a `HistoryLookup`:
- `Changed(value)` holds the before-value from the first change after `B`.
- `Unchanged { h }` means no archived change after `B`. The caller resolves the value against the node's state at `H` with `or_state` or `code_or_state`, so the node's own changesets cover blocks above `H`.

If `B < E`, the result is `HistoryPruned`. Bytecode is always read from the node.

## Deviations from the TDD

- **Segment build.** Files are produced by reth's `StaticFileProviderRW` in a temporary directory and then uploaded. Object names drop reth's block-range suffix (for example, `static_file_headers_0_3` becomes `headers`). The reader restores the reth names in its cache directory before opening a segment. The format is unchanged.
- **Wire encodings.**
  - Headers are RLP and transactions are EIP-2718.
  - Receipts and changesets use reth's `Compact` encoding.
  - `block_bodies.idx` is a fixed-width custom file, not a `NippyJar`.
- **Dictionaries.** No zstd dictionaries are used. `catalog.dictionaries` is always empty.
- **CAS backends.** CAS runs on S3, `MinIO` and `InMemory`. `LocalFileSystem` has no conditional update, so catalog CAS returns `BackendUnsupported`. `LocalFileSystem` stays usable for segments and the cache, but only with `SlateDB` GC disabled.
- **Changesets.** Segments carry account and storage changesets only, the same as reth. The proof history is out of scope, as the TDD states.
- **Default prefix.** `--archive.url` uses the URL path as the prefix. There is no implicit `<chain-id>/` default.
- **Range byte cap.** `DEFAULT_MAX_RANGE_BYTES` is 64 `MiB`, not the TDD's 256 MB. The cap counts raw payload bytes, but the response is hex JSON (about 2x), and reth's default RPC response limit is 160 `MiB`, so 256 MB ranges could never be returned. The writer's client accepts responses up to 512 `MiB`.
- **Segment object layout.** The TDD (§2.5) stores objects at `seg/<k>/<file>`; this crate stores them at `seg/<k>/<epoch>/<file>`. Reason: S3 DELETE is unconditional, so with a single path a stale GC's delete could remove objects a newer owner just re-uploaded and published (see "Epoch-scoped objects and GC"). File bytes are unchanged and byte-identical across rebuilds; only the directory differs.
- **Catalog fields.** `writer_epoch` (fencing) and `retired` are additions to the TDD's catalog. Old catalogs without them parse with defaults.

## Tests

The tests are hermetic and in-memory, except for the `MinIO` test:

```sh
cargo test -p base-history-store --lib --features test-utils
```

They cover:
- Catalog CAS, the regression rules and epoch claims, and two writers opening concurrently in both orders.
- Byte-identical rebuilds, including after a restart with a partial upload, after which no object of the crashed epoch remains.
- Each verifier failure.
- Writer fencing (including a writer paused after its index flush), divergence halts, chunk clamping and halving, zero sizes and malformed confirm answers.
- Orphan GC within its window, GC fencing, a GC paused at its DELETE while a newer owner publishes, and the referenced epoch directory being kept.
- Retirement, sweep after grace, crash recovery at each sweep step, per-pass caps and pruner fencing.
- Routing and range splits.
- Reader parity against the fixture for blocks, receipts, logs and state.
- Index watermark refusals when the catalog is ahead of the index view, number-based reads of uncovered segments, and a hash lookup racing a real `SlateDB` view refresh.
- Corrupt-object, corrupt-changeset and tampered-sender detection.
- Cache namespacing, single-flight fills, byte budget, oversized segments and corrupt disk entries.
- Leased disk entries surviving eviction pressure, concurrent fills sharing the budget, and cancelled fills cleaning up.
- Cache restarts: abandoned fills removed, and persisted entries reconciled against a smaller budget, both checked against measured filesystem bytes.
- Confirm fetches that return another block's header.

Interleavings are driven by `test_utils::GatedStore`, an `ObjectStore` wrapper that pauses chosen operations on a path prefix until the test releases them; no test sleeps to order events.

The opt-in `MinIO` run covers the writer, reader, CAS race, retirement, sweep, orphan GC and catalog fencing against a real S3 API:

```sh
podman run -d --rm --name history-store-minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  cgr.dev/chainguard/minio:latest server /tmp/data
podman exec history-store-minio mkdir -p /tmp/data/history
HISTORY_STORE_MINIO_ENDPOINT=http://127.0.0.1:9000 HISTORY_STORE_MINIO_BUCKET=history \
AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1 \
  cargo test -p base-history-store --features test-utils --test minio -- --ignored
podman stop history-store-minio
```

The `minio/minio` image works as well, where your registry allows it.

## Benchmarks

`benches/history_store.rs` is a criterion scaffold. It measures:
- Segment build time.
- A cold block read, where the segment is fetched from the in-memory store.
- A warm block read, served from the cache.

```sh
cargo bench -p base-history-store --features test-utils --bench history_store
```

These numbers leave out object-store latency. The TDD §8 measurements (`SlateDB` point gets against a 500 GB index, writer throughput, dictionary ratio, historical `eth_call` latency) still need a real bucket and production-sized data.
