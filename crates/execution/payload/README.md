# `base-execution-payload-builder`

Payload builder for Base.

## Overview

Implements Base payload building and validation for the Base execution node. The
`BasePayloadBuilder` assembles new execution payloads from transaction pool contents and
`BasePayloadBuilderAttributes` received from the consensus layer. `BaseExecutionPayloadValidator`
verifies
built payloads against consensus rules. Also provides data availability configuration via
`BaseDAConfig` for fee calculation.

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

## Sealed-block observations

Every block the builder composes from the transaction pool emits one `sealed block observation`
log line (target `payload_builder`) and one sample on each `base_builder_sealed_block_*` histogram.
The line logs at `debug`, or at `warn` with `slow=true` when the state-root time meets or exceeds
`SLOW_BLOCK_STATE_ROOT` (50 ms), so slow blocks are logged with their contents by default.
Metrics are recorded for every block regardless of level.

The observation joins the block's state-root time to what drove it, so resource metering can be
calibrated against builder state-root cost:

- block number, hash, transaction count, and gas used;
- state-root time from just before handling the state-root job (if any) through `builder.finish`,
  time spent waiting on the job, and how the root was computed (`task`, `synchronous`, or
  `task_fallback`);
- hashed post-state sizes: accounts, storages, total slots, and the account with the most changed
  slots (unhashed address and slot count);
- trie update sizes: account nodes, storage tries, storage nodes, and removed nodes;
- the block's usage of each resource-metering dimension, when metering is active.

The existing `native_payload_state_root_duration` measures total native build time minus execution
time, including post-execution work outside this interval. Its canary measurements provide an
indirect comparison, not a measured warning rate for the new timer.

Replayed `no_tx_pool` blocks are not observed. The builder calls `SealedBlockObservation::log` and
`BuilderMetrics::record_sealed_block` once per sealed block. The counts iterate the block's maps
once and never run in the per-candidate loop.

```text
WARN payload_builder: sealed block observation block_number=7 block_hash=0xabababababababababababababababababababababababababababababababab tx_count=2 gas_used=42000 slow=true state_root_source=task state_root_us=140000 state_root_wait_us=250 hashed_accounts=3 hashed_storages=2 hashed_storage_slots=4 largest_storage_slots=3 largest_storage_address=0x0101010101010101010101010101010101010101 trie_account_nodes=2 trie_storage_tries=2 trie_storage_nodes=3 trie_removed_nodes=3 resource_usage=cpu=5,state_root=11
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
