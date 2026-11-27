# base-exoware-archive-rpc

Node wiring for `base-exoware-archive`:

- `ProviderSource`, the reth provider as the exporter's `BlockSource` and the reader's
  `NodeState`.
- The exporter: a supervised task that publishes finalized segments.
- The archive-backed `eth_` reads behind `ArchiveRpcBoundary`.
- The `--archive.exoware.*` arguments (`ExowareArchiveArgs`).

`ExowareArchiveExtension` is installed by `base-execution-cli` after `TxBaseRpcExtension`.
Without `--archive.exoware.url` it is a no-op and the node is unchanged.

## Arguments

| Flag | Default | Meaning |
| --- | --- | --- |
| `--archive.exoware.url` | unset | Store URL; unset disables the archive |
| `--archive.exoware.namespace` | `base-archive/` | Key namespace (`[a-z0-9-]+/`) |
| `--archive.exoware.export` | off | Run the exporter on this node |
| `--archive.exoware.provision` | off | Write the config row at exporter start if absent; an existing row must match |
| `--archive.exoware.segment-blocks` | 8192 | Blocks per segment, 1..=65536 |
| `--archive.exoware.start-block` | 0 | First archived block, segment-aligned |
| `--archive.exoware.retain-segments` | unset | Keep only the most recent segments |
| `--archive.exoware.timeout` | 5s | Store budget per RPC request and per exporter store call |
| `--archive.exoware.prune-grace-segments` | 2 | Segments kept below the earliest served one before rows are deleted, ≥ 1 |
| `--archive.exoware.export-batch-blocks` | 256 | Blocks the exporter loads, encodes and writes at a time, 1..=65536; bounds its block memory |
| `--archive.exoware.min-retention-margin` | 4096 | Blocks of history the node must keep beyond one segment (see below) |

The node refuses to start when the archive cannot cover its retention. The archive's head `H`
trails the node's head by the finality lag plus up to one segment, blocks above `H` are not
archived yet, and keys unchanged since `H` are read from the node's own history at `H`. So each
of account history, storage history, bodies, receipts and the transaction lookup must be
unpruned, pruned before a fixed block, or kept for more than `segment-blocks` +
`min-retention-margin` blocks; `full` pruning is refused. The default margin of 4096 blocks
covers L1 finality (~13 minutes) at 200 ms blocks; at 2 s blocks about 512 suffices.

The store API key is read by the Exoware SDK from `EXOWARE_API_KEY` only. There is no flag
and it is never logged. There is no audit-interval flag; audits are on demand
(`SegmentAuditor`).

## Exporter

With `--archive.exoware.export` a node-started hook spawns the exporter on the node's task
executor with a graceful-shutdown signal; node shutdown waits for the round in flight. It
provisions the config row when asked, retries an unreachable store with backoff, opens
`SegmentPublisher` (which requires a byte-equal config row) and runs
`SegmentPublisher::run_until`. A config mismatch, missing config, quarantined segment or
oversized value stops the exporter with an error log; the node keeps serving.

Each segment is streamed `--archive.exoware.export-batch-blocks` blocks at a time, so the
exporter holds at most one batch of blocks plus the segment's index rows and postings (see
`base-exoware-archive`, "Exporter memory"). The marker digest is computed per family as the
rows stream ("Marker digest").

The exporter does not gate reth's pruning. If it lags the node's retention it cannot publish
the pruned blocks: it logs an error, keeps retrying, and `lag_blocks` (finalized block minus
the last published block) grows. Alert on `lag_blocks`; another node with the blocks must
publish the gap.

## Reads

Every method asks the node first. The archive is used only when the node's own data for the
request is pruned:

| Method | Goes to the archive when | Archive path |
| --- | --- | --- |
| `eth_getBlockByNumber`, `eth_getBlockByHash`, `eth_getBlockReceipts`, `eth_getTransactionByBlock{Hash,Number}AndIndex` | The node answered 4444, or returned `null` for a pruned number, or its answer is visibly incomplete over pruned data: a deposit lacks a field its fork requires (`depositNonce` from Regolith, `depositReceiptVersion` from Canyon; Base defaults both when the receipt is gone) and a fresh retention read shows its receipts or lookup pruned. A complete answer is served as is, with no retention read. A block hash the node does not know keeps the node's answer with no store call: the node never prunes headers and the archive holds only finalized canonical blocks, so an unknown hash is a block the node has not seen yet or a forked-out one, never pruned history. | The verified block at the node's number for it |
| `eth_getBlockTransactionCountBy{Number,Hash}` | 4444 or `null` for a pruned number, against the earliest body only | Same |
| `eth_getTransactionByHash`, `eth_getTransactionReceipt` | As for blocks. A `null` stays local without a store call when the hash is in the local pool or the node's `TransactionLookup` is unpruned and has no entry for it, so pending-transaction polling never reaches the store; a transaction the node still places is read at that block. | Tx locator, then the verified block |
| `eth_getLogs` | The node answered 4444 (for `blockHash`, only for a block the node knows) | Blocks below the node's first complete block from the archive, the rest from the node's filter, no gap. reth's `--rpc.max-blocks-per-filter` and `--rpc.max-logs-per-response` apply to the combined result with reth's errors; the archive reader stops after the 32-block chunk that crosses the log cap. |
| `eth_getBalance`, `eth_getTransactionCount`, `eth_getCode` | The block (number, hash, `earliest`; EIP-1898 objects included) is at or below the `AccountHistory` checkpoint, or the node refused the read as pruned | State history seek, else the node's state at the archive head `H` |
| `eth_getStorageAt` | Same, against `StorageHistory` | Same |

`latest`, `pending`, `safe` and `finalized` never reach the archive, and neither does a number
above what the node holds. `eth_getTransactionCount` with a non-zero `nonce_key` is always
answered by the Base transaction override; every local protocol-nonce read also goes through
it, so the override's behavior is unchanged.

Errors map once, in `ArchiveRpcBoundary`:

- A number-addressed request in the pruned range, or a hash that resolved in the archive,
  gets 4444 `pruned history unavailable: <reason>` when the archive cannot answer (outage,
  timeout, integrity fault, retention moved). Never `null` or empty data.
- A block hash the node does not know gets the node's own answer without a store call.
- A transaction hash the node cannot place returns `null` for a genuine archive miss, and also
  when the store failed before the hash resolved (`hash_fallbacks` metric, warning log), so
  polling for a pending transaction does not turn into errors during an outage.
- Log query limits are `-32602` with reth's messages.

A local read that fails with a server error (reth's static-file pruner can delete a file
under an in-flight read) rereads retention fresh: if the block is now below the node's
boundary the archive answers under the same rules, otherwise the node's error is returned
unchanged.
A node that lacks the canonical hash of an archived block cannot verify it and answers 4444.
Deposit nonces of the Regolith window (`depositNonce` in transactions and receipts) are
trusted-store fields, like state and log completeness: the receipt root does not cover them and
the reader only checks them for consistency with the archived account changes (see
`base-exoware-archive`).
Responses are built by `ArchiveConverter` with the node's own Base response constructors, so
they equal what the node served before it pruned.

Each RPC request reaches the archive at most once: `ArchiveEth`'s local calls use
`BaseEthApi::without_pruned_history()`, so the execution hook below never reads the archive for
a request `ArchiveEth` already routes. The `requests` counter counts archive read requests begun.

## Historical execution

`eth_call`, `eth_callMany`, `trace_block`, `debug_traceTransaction` and
`debug_traceBlockByNumber` run reth's own EVM for pruned blocks. The extension installs
`ExowareHistory` as `BaseEthApi`'s `PrunedHistory` hook, which takes over only at the execution
seam (`Call::spawn_with_state_at_block`), the recovered-block loader and the transaction+block
resolver:

- State: the node opens its own state first and reads it once. If it refuses with
  `StateAtBlockPruned` or `InsufficientChangesets`, the execution runs once on
  `ArchiveStateProvider` (account, storage and code at the end of block N, from the archived
  before-values and the node's bytecodes and canonical hashes). Other errors are unchanged.
- Async store reads run on a dedicated `archive-exec` runtime. One absolute deadline
  (`--archive.exoware.timeout` from the start of the request) covers the state open, the
  blocking-task wait, every read and the marker recheck; finishing late is 4444.
- Any archive failure during execution, including one a tracer swallows (`prestateTracer`
  code reads), and a failed marker recheck return 4444, never a result.
- Pruned bodies (missing or `BlockExpired`) load as verified archived blocks with no BAL.
  Transactions keep the node's location when it has one; the archive locator is used only
  when the node cannot place the hash.

Limits: results are only as trustworthy as the archived state rows (there is no state root to
check them against). Each uncached slot is at least 2 sequential store round trips, so large
traces are slow on a remote store. With the hook installed, every execution except `pending`
pays one extra local account read before it starts (the probe that makes reth decide whether
the state is pruned). reth resolves `latest`, `safe`, `finalized` and `earliest` to a block hash
before an execution starts, so `eth_call` at `latest` pays it too; only a caller that passes the
literal `latest` tag to the execution seam skips it. State roots, proofs and witnesses are unsupported (4444), and so is
`hashed_post_state` for a destroyed account.

## Gaps

- `eth_estimateGas`, `eth_createAccessList`, `eth_simulateV1`, `trace_filter`,
  `debug_storageRangeAt` and `eth_getProof` over pruned history are not tested and may refuse.
- Uncle methods, log filters (`eth_newFilter`) and subscriptions are not routed.
- Retention reads are cached for 2 s (`RETENTION_TTL`), and an answer whose deposit fields
  are present is never rechecked. A visibly incomplete answer always rereads retention, so
  defaulted deposit fields over pruned receipts are never served.

## Tests

```sh
cargo test -p base-exoware-archive-rpc
cargo test -p base-execution-cli --lib standard_node
```

- `pruned_node_serves_history_from_archive`: a dev node with 64-block retention exports to an
  in-process simulator; every block, transaction, receipt, log and state response captured
  before pruning is JSON-equal after pruning. With the simulator stopped, number-addressed
  reads are 4444 and unknown hashes stay `null`. A forged block-hash locator for a hash the
  node does not know is never read: the node's answer stands.
- `block_reads_over_pruned_bodies_make_one_archive_request` (its own binary, counting the
  process-wide `requests` metric): a block read over pruned bodies makes exactly one archive
  request, also with the store down, and an unknown block hash makes none across all five
  hash-addressed block methods.
- `unpruned_node_with_store_down_answers_locally`: misses and recent reads stay local.
- `transaction_count_override_is_preserved`: `nonce_key` answers equal the Base override's.
- Unit tests cover the error boundary, hash fallback, routing per checkpoint, the retention
  cache and argument parsing.
