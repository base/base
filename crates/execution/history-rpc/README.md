# base-history-rpc

Node-side wiring for the shared chain history store (archive). It has two parts:
- The `archive_` export endpoint that the writer pulls from.
- An `eth_` override that serves locally pruned history from the archive bucket.

Both are `BaseNodeExtension`s, installed by `base-execution-cli` from `--archive.*` flags. When the flags are absent, both extensions are no-ops.

## Export (`archive_`, TDD §3)

`ArchiveExportExtension` is enabled by `--archive.enable`. It serves two methods:
- `archive_status`, which returns `{head, safe, finalized, earliestLocal, maxRangeBlocks, maxRangeBytes}`.
- `archive_getBlockRange(from, to)`, which returns reth's own rows for each block, encoded as `ArchiveBlock`.

A range is rejected with a typed code when:
- It ends above finalized (`4001`).
- It starts below the node's earliest local block or below a prune checkpoint of an exported segment (`4002`).
- It exceeds `--archive.max-range-blocks`, default 8192 (`4003`).
- It exceeds `--archive.max-range-bytes`, default 64 MiB (`4004`). The TDD says 256 MB; see `base-history-store`'s deviations for why it is lower.

Other failures are an invalid range (`4005`), a missing block (`4006`) and missing changesets (`4007`). Error data carries `finalized` and `earliestLocal`.

## Reader routing (`eth_`, TDD §5)

`ArchiveEthExtension` is enabled by `--archive.url`, with optional `--archive.endpoint`, `--archive.cache-dir` and `--archive.cache-size`. At startup it reads the catalog once, bounded by 10 s, on a scoped thread. A reachable catalog for another chain id, genesis or format fails node start. A missing catalog, unreachable storage or a timeout logs a warning; the node starts and opens the reader lazily on the first archive-bound request. It replaces these methods with local-first, archive-on-miss versions:

| Method | Local miss signal | Archive path |
| --- | --- | --- |
| `eth_getBlockByNumber`, `eth_getBlockByHash` | `null`, error code 4444, a block below the node's earliest block, or a block at or below the `Receipts` prune checkpoint | `b‖` for hashes, then the segment's header and transactions |
| `eth_getTransactionByHash`, `eth_getTransactionReceipt` | Same as the block methods | `t‖`, then the segment |
| `eth_getLogs` | Error code 4444 | The range is split at `H`: blocks at or below `H` come from header blooms and then hit receipts, and blocks above `H` go to the node's filter. The archive part is capped at `MAX_ARCHIVE_LOG_RANGE` (10,000 blocks). |
| `eth_getBalance`, `eth_getTransactionCount`, `eth_getCode` | The block id, resolved through the node (`earliest`, `safe`, `finalized`, hashes), is at or below the `AccountHistory` prune checkpoint | `h‖` seek to the changeset. On `Unchanged`, the value is read from the node's state at `H`. Bytecode always comes from the node. |
| `eth_getStorageAt` | Same, against the `StorageHistory` checkpoint | Same |

Pruned or unavailable archive answers fall back to the node's own answer, for state reads too. Any other archive failure becomes `INTERNAL_ERROR` and is never returned as data. Errors map to transport codes once, in `ArchiveRpcBoundary::error`:
- `HistoryPruned` and `Unavailable` map to 4444.
- Everything else maps to internal.

Responses are built by `ArchiveConverter`, using the same Base RPC types the node uses. That includes:
- Deposit nonce and receipt version, read from archived receipts.
- L1 fee fields, via `extract_l1_info`.
- Millisecond timestamps, via `BaseTimeCache`.

### `eth_getTransactionCount` ownership

When the archive is on, `Eip8130RpcExtension` gets `TransactionCountOwner::Archive` and drops its `eth_getTransactionCount`, so this extension owns the method. In that case:
- A non-zero `nonce_key` is delegated to `Eip8130EthApiOverrideServer::get_transaction_count` unchanged.
- Everything else goes local first, then to the archive.

`ArchiveArgs::transaction_count_owner` in `base-execution-cli` makes the decision. Install order does not change the outcome.

## Deviations and gaps

- **Historical `eth_call` is not implemented (stretch).** `eth_call`, `eth_estimateGas`, `debug_trace*` and `eth_createAccessList` at a locally pruned block still return the node's own error. Implementing them needs a reth `StateProvider` backed by the archive, which this proof of concept does not include.
- **Detecting pruned state.** The override first compares the requested block against the prune checkpoint of the domain the read needs. reth's historical state provider checks both domains' checkpoints and the changeset files, so with unequal retention it can refuse a read whose own domain is retained. Such a refusal also goes to the archive: either the 4444 code (`InsufficientChangesets`, `BlockExpired`) or `StateAtBlockPruned` for the requested block, which reth maps to an internal error and which is recognised by comparing with reth's own conversion of that error. Successful local answers and other errors are kept.
- **Incomplete local answers.** reth's block cache can keep serving a pruned block, and Base fills deposit fields from receipts and defaults them when receipts are pruned. A local block, transaction or receipt below `earliest_block_number`, or at or below the `Receipts` prune checkpoint, is replaced by the archive's copy (`LocalRetention::answer_incomplete`).
- **Per-request provider reads.** Routing reads the earliest block and three prune checkpoints per request (`LocalRetention::read`). They are small MDBX reads; they are not cached.
- **Methods not routed.**
  - `eth_getBlockReceipts` and `eth_getBlockTransactionCountBy*`.
  - `eth_getTransactionByBlock*AndIndex` and `eth_getUncle*`.
  - Logs filters (`eth_newFilter` and similar) and subscriptions.

## Tests

```sh
cargo test -p base-history-rpc --lib --test archive_e2e
```

The unit tests cover per-domain state routing, receipt-only pruning, the error boundary and recognising a `StateAtBlockPruned` refusal.

- **`pruned_node_serves_history_from_archive`** checks node → writer → reader parity:
  - One node serves the export and the override. Its retention is 64 blocks, with static files of 8 blocks each.
  - Six blocks of deployments, transfers, storage writes and logs are recorded for all nine methods.
  - The chain then grows past retention while an in-process writer archives it.
  - The test asserts that the node has pruned the block 1 body and the state at block 1.
  - Every response must equal the pre-pruning JSON exactly.
  - Afterwards, `eth_getBalance(alice, "earliest")` must equal the balance at the node's earliest block number and differ from genesis.
- **`unequal_retention_serves_refused_state_from_archive`**: the same parity run with account history retained and storage history pruned. The node refuses account reads at old blocks although their own checkpoint is clear; every response still matches.
- **`startup_refuses_foreign_catalog_and_tolerates_unreachable_storage`**: a reachable catalog for another chain fails `build()`; an S3 endpoint nobody listens on lets the node start and serve local reads.
- **`nonzero_nonce_key_matches_eip8130`**: with the archive on, a non-zero `nonce_key` returns the same value as the EIP-8130 override. Before Everest it returns the same `-32602` error.
- **Archive off.** `eip8130-rpc-node/tests/rpc.rs` covers this case: `nonce_key_zero_returns_protocol_nonce` and the channel-key tests run with `TransactionCountOwner::Eip8130`.
