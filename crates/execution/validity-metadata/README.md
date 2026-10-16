# `base-execution-validity-metadata`

Durable, builder-local records of which transactions in each pool-built block were validity
transactions.

## Overview

A block builder classifies each committed pool transaction that carried nonempty validity
predicates (including transactions whose EVM execution reverted) and hands one
`ValidityBlockRecord` per sealed block to a `ValidityMetadataRecorder`. A dedicated writer thread
commits records to a [redb](https://docs.rs/redb) `ValidityMetadataStore`, which serves
`base_getValidityTransactionStatus` through `base-txpool-rpc`.

- **`ValidityBlockRecord`** — exact block hash, block number, block transaction count and the
  strictly ascending hashes of its validity transactions. Every recorded pool-built block gets a
  record, including blocks with no validity transactions, so an absent record (unknown) is never
  confused with a recorded `false`. Forced and attribute transactions count toward
  `transaction_count` but are never validity transactions. Predicates, authorization signatures
  and uncommitted (deferred, expired, rejected) transactions are never stored.
- **`ValidityMetadataStore`** — cloneable handle; `open`, `get` and `put_batch` perform blocking
  disk I/O and must run off async executor threads.
- **`ValidityMetadataRecorder`** — cloneable producer handle. The default value is disabled and
  behavior-neutral. `spawn` starts the writer thread; `record` never blocks, never touches disk and
  reports `Queued`, `Disabled`, `QueueFull` or `Closed`.

## Storage format

Records are keyed by exact block hash. Format version 1 is
`[version (1) | block hash (32) | block number (8 BE) | transaction count (4 BE) |
validity count (4 BE) | validity hashes (32 each, strictly ascending)]`, with at most
`MAX_VALIDITY_TRANSACTIONS_PER_RECORD` (65,536) hashes, so one record is at most about 2 mebibytes and a
lookup reads one record and binary-searches it. Truncated records, unknown versions, length or
count mismatches, unsorted hashes and key mismatches are returned as `Corrupt` errors, never as a
classification.

Each `put_batch` is one atomic write transaction committed with immediate durability. An
identical retry is a no-op. A different record for an already stored block hash never overwrites
it: the hash is durably marked conflicting and `get` returns `Conflict` for it permanently. This is
an intentional fail-safe; there is no repair tooling, and deleting the database is not a routine
repair because it discards every other record. Only pool-built blocks may be recorded: payload
re-execution (`no_tx_pool`), witness generation and follower nodes must never record.

## Delivery and availability

The queue holds at most the capacity passed to `spawn`
(`DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY` is 1,024 records) and the writer commits at
most `MAX_VALIDITY_METADATA_WRITE_BATCH` records per transaction. Records dropped on a full or
closed queue, records in a failed write and records still queued at crash or shutdown are lost and
not retried; lookups for those blocks report metadata as unavailable, never `false`. Drops, writes
and conflicts are counted in the `builder.validity_metadata.*` metrics. Drop, invalid-record,
conflict and write-failure logs are emitted only on the first and every power-of-two occurrence
(`ValidityMetadataMetrics::is_log_sampled`). The writer thread exits after every recorder
clone is dropped and the queue is drained.

Records are never pruned: disk usage grows with the number of recorded blocks, and capacity and
backups are the operator's responsibility. redb is single-process: share one opened store between
the recorder and the RPC within a process and never open the same file from two processes.

Records are builder-reported classifications, not cryptographic proofs. They are not derived from
canonical chain data, so blocks built elsewhere, blocks built before recording was enabled and
lost records have no metadata and cannot be backfilled.

## Boundaries

- Builder to writer: an in-process channel bounded to the configured queue capacity; at capacity
  `record` returns `QueueFull` without blocking. Each queued record is one bounded per-block
  allocation: at most one hash per committed block transaction.
- Writer to redb: a record above `MAX_VALIDITY_TRANSACTIONS_PER_RECORD` hashes
  (`MAX_VALIDITY_RECORD_LEN` bytes, about 2 mebibytes) is dropped as invalid before writing, far below
  redb's per-value limit. The block it describes reports metadata as unavailable.
- redb to RPC: one lookup reads and validates at most one `MAX_VALIDITY_RECORD_LEN` record and
  returns a fixed-size JSON object.

## Usage

```rust,ignore
use base_execution_validity_metadata::{
    DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY, ValidityBlockRecord, ValidityMetadataRecorder,
    ValidityMetadataStore,
};

let store = ValidityMetadataStore::open(path)?;
let (recorder, _writer) =
    ValidityMetadataRecorder::spawn(store.clone(), DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY)?;

// After a pool-built block is sealed:
let _status = recorder.record(ValidityBlockRecord::new(
    sealed_hash,
    sealed_number,
    transaction_count,
    validity_tx_hashes,
));
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
