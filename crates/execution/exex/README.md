# `base-execution-exex`

Execution extensions (`ExEx`) for Base.

## Overview

Implements a live Execution Extension that collects and stores Merkle Patricia Trie proofs
during block execution for use in fault-proof generation. Runs alongside the execution node as a
background task, capturing state proofs incrementally and pruning data outside the proof window.
Supports both batch sync at startup and real-time collection during normal operation.

## Problem

Reth's default `eth_getProof` implementation works by reverting in-memory state diffs backward
from the current tip. For blocks older than ~7 days, this causes unbounded memory growth and
out-of-memory (OOM) crashes — a critical issue for infrastructure serving rollup fault proofs
and indexers that query historical state.

## Solution

The proofs-history `ExEx` implements a proof-state store that tracks Merkle Patricia Trie nodes,
hashed accounts, and hashed storage within a configurable retention window. This enables direct
lookups of proofs at historical blocks without reverting state.

The `ExEx` processes blocks asynchronously, so it adds zero overhead to sync speed and negligible
tip latency.

## Architecture

```
base-reth-node
├── Standard reth pipeline (sync, EVM, state)
├── proofs-history ExEx (ingests committed blocks → proofs-history store)
├── Pruner task (background, removes data outside retention window)
└── RPC overrides (eth_getProof, debug_executePayload, debug_executionWitness)
```

The store lives in a separate proofs-history database. RocksDB is the default backend and uses a
V2 schema with current-state column families plus before-change history rows. MDBX remains
available with versioned history tables.

RocksDB V2 maintains these logical column-family groups:

| Group                  | Contents                                                |
| ---------------------- | ------------------------------------------------------- |
| Current state          | Latest account/storage leaves and trie branch nodes     |
| History-before rows    | Values before each block changed a logical key          |
| `V2BlockChangeSet`     | Block-number ordered index for prune and unwind         |
| `V2ProofWindow`        | Earliest/latest proofs-history block pointers           |

MDBX maintains four history tables:

| Table                  | Contents                                                |
| ---------------------- | ------------------------------------------------------- |
| `AccountTrieHistory`   | Branch nodes of the account trie, versioned by block    |
| `StorageTrieHistory`   | Branch nodes of per-account storage tries, by block     |
| `HashedAccountHistory` | Account leaf data (balance, nonce, etc.), by block      |
| `HashedStorageHistory` | Storage slot values, versioned by block                 |

A block change-set index enables efficient pruning: given a block number, the pruner knows exactly
which keys were modified and can delete only those entries.

Legacy RocksDB V1 proofs-history databases are not migrated in place. If a node was using the old
RocksDB layout, use a fresh `--proofs-history.storage-path` and rebuild the proof history.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-exex = { workspace = true }
```

The `ExEx` is installed into the node builder during node setup and runs automatically as blocks
are processed.

## Verification

After the node starts, query the sync status of the proofs store:

```
debug_proofsSyncStatus → { "earliest": <block>, "latest": <block> }
```

Once `latest` tracks the chain tip, `eth_getProof` calls for every block within
`[earliest, latest]` will be served from the proofs-history store.

## Operational Commands

**Prune** — manually remove old proof history to reclaim space:

```bash
base-reth-node proofs prune \
  --datadir /path/to/reth-datadir \
  --proofs-history.storage-path /path/to/proofs-db \
  --proofs-history.window 1296000
```

**Unwind** — recover from corruption by reverting to a specific block:

```bash
base-reth-node proofs unwind \
  --datadir /path/to/reth-datadir \
  --proofs-history.storage-path /path/to/proofs-db \
  --target <BLOCK_NUMBER>
```

> You can only unwind to a block after the earliest block number in the database.

## Metrics

When the `metrics` feature is enabled, the proofs-history system exposes Prometheus metrics.

### Block processing (`base_trie.block.*`)

| Metric                               | Type      | Description                                        |
| ------------------------------------ | --------- | -------------------------------------------------- |
| `total_duration_seconds`             | Histogram | End-to-end time to process a block                 |
| `execution_duration_seconds`         | Histogram | Time spent in EVM execution                        |
| `state_root_duration_seconds`        | Histogram | Time spent calculating state root                  |
| `write_duration_seconds`             | Histogram | Time spent writing trie updates to storage         |
| `account_trie_updates_written_total` | Counter   | Number of account trie branch nodes written        |
| `storage_trie_updates_written_total` | Counter   | Number of storage trie branch nodes written        |
| `hashed_accounts_written_total`      | Counter   | Number of hashed account entries written           |
| `hashed_storages_written_total`      | Counter   | Number of hashed storage entries written           |
| `earliest_number`                    | Gauge     | Earliest block number in the proofs store          |
| `latest_number`                      | Gauge     | Latest block number in the proofs store            |

### Pruner (`base_trie.pruner.*`)

| Metric                          | Type      | Description                                        |
| ------------------------------- | --------- | -------------------------------------------------- |
| `total_duration_seconds`        | Histogram | Duration of each prune run                         |
| `pruned_blocks`                 | Gauge     | Number of blocks pruned in the last run            |
| `account_trie_updates_written`  | Gauge     | Account trie entries deleted in the last prune     |
| `storage_trie_updates_written`  | Gauge     | Storage trie entries deleted in the last prune     |
| `hashed_accounts_written`       | Gauge     | Hashed account entries deleted in the last prune   |
| `hashed_storages_written`       | Gauge     | Hashed storage entries deleted in the last prune   |

### RPC (`base_rpc.eth_api_ext.*`)

| Metric                          | Type      | Description                                        |
| ------------------------------- | --------- | -------------------------------------------------- |
| `get_proof_latency`             | Histogram | Latency of successful `eth_getProof` requests      |
| `get_proof_requests`            | Counter   | Total `eth_getProof` requests received             |
| `get_proof_successful_responses`| Counter   | Total successful `eth_getProof` responses          |
| `get_proof_failures`            | Counter   | Total failed `eth_getProof` requests               |

### Storage operations (`base_trie.storage.operation.*`)

Per-operation `duration_seconds` histograms are recorded for: `store_account_branch`,
`store_storage_branch`, `store_hashed_account`, `store_hashed_storage`,
`trie_cursor_seek_exact`, `trie_cursor_seek`, `trie_cursor_next`, `trie_cursor_current`,
`hashed_cursor_seek`, `hashed_cursor_next`.

## Performance

Benchmarked on Base Sepolia (~700k block window, WETH contract):

| Metric         | Value                                        |
| -------------- | -------------------------------------------- |
| Avg latency    | ~15 ms per `eth_getProof`                    |
| Throughput     | ~5,000 req/s (10 concurrent workers)         |
| Sync overhead  | Zero (`ExEx` processes asynchronously)       |
| Memory         | Bounded by window size — no OOM risk         |

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
