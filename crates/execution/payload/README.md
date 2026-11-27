# `base-execution-payload-builder`

Payload builder for Base.

## Overview

Implements Base payload building and validation for the Base execution node. The
`BasePayloadBuilder` assembles new execution payloads from transaction pool contents and
`BasePayloadBuilderAttributes` received from the consensus layer. `BaseExecutionPayloadValidator`
verifies
built payloads against consensus rules. Also provides data availability configuration via
`BaseDAConfig` for fee calculation.

## Storage-trie prefetch

`StoragePrefetchPool` is an opt-in, read-only prefetch of storage-trie pages for the serial
state root. `--builder.storage-prefetch` (off by default) is its only flag; its sizes are
constants on `StoragePrefetchPool`, documented with the measurements behind them. While a block
executes, a state hook counts each contract's distinct changed slots, updated or created, since
the root walks the trie path of a new leaf as it does of an updated one. Once a contract passes
the gate (`GATE_SLOTS`, 32), its changed slots go to a pool of `WORKERS` (16) threads in chunks
of `CHUNK_SLOTS` (32). Each worker builds a storage proof for its chunk on the parent state,
through the same `state_by_block_hash` provider the build uses, and drops it, which faults in the
trie pages the state root reads next. A contract's last partial chunk is sent when the build loop
stops selecting transactions, before the root starts. The proof never feeds the block, so the
payload is identical with the prefetch on or off. Builds that use a parallel state-root job skip
the prefetch, since that job reads the trie during execution itself.

The execution thread never waits: the queue (`QUEUE_CHUNKS`, 256 chunks) drops chunks when full
and counts them. When the build ends, workers skip its queued chunks unread and its parked state
providers are freed on a release thread, off the build path; a chunk already being read releases
its provider when its proof returns. Metrics are under
`base_builder_storage_prefetch_*`. `base_builder_storage_prefetch_block_state_root_duration`
takes the `base_builder_native_payload_state_root_duration` sample (total build time minus
execution) of each block this builder composed from the pool that issued or dropped a prefetch
chunk; `no_tx_pool` replays record neither. To measure the effect, compare the same qualifying
blocks, or blocks with the same heavy writes, with the flag on and off, rather than its
percentiles against the native metric's across all blocks. Each sealed block that prefetched
also logs one debug-level `storage-trie prefetch` line with its slot counts, the head start the
trigger gave (`trigger_lead`), late chunks and `sealed_after`, the time from the end of execution
until the block was sealed.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-payload-builder = { workspace = true }
```

```rust,ignore
use base_execution_payload_builder::BasePayloadBuilder;

let builder = BasePayloadBuilder::new(evm_config, payload_validator);
let payload = builder.build_payload(attrs, best_payload)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
