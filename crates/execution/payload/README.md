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

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).

## Native payload finalization timings

`reth_base_builder_native_payload_finalization_stage_seconds{stage}` measures work in successful
transaction-pool builds. Replays (`no_tx_pool`) are excluded. Skipped operations have no sample,
not a zero. The existing `native_payload_state_root_duration` remains **post-execution to sealed**;
it is not a root-only timer.

| Stage | Timed operation |
| --- | --- |
| `merge_transitions`, `hash_post_state` | Merge execution changes; hash the resulting account and storage changes. |
| `synchronous_root` | State provider's `state_root_with_updates` call. Also recorded on fallback after a failed worker. |
| `root_worker_active`, `root_worker_idle` | Sparse trie worker wall time outside vs. inside waits for updates, proofs or storage jobs. Their sum per build is worker elapsed time, not aggregate CPU time. |
| `root_wait` | Time the builder blocks obtaining the parallel result, including an unsuccessful wait before fallback. |
| `transactions_root`, `receipts_root`, `logs_bloom` | Individual header commitments computed during assembly. |
| `message_root` | Message passer storage root when the fork requires it. |

The worker runs concurrently with execution: do not add its times to `root_wait` or to build time.
`finish_payload` remains the encompassing assembly trace span; it excludes the parallel wait.

After deployment, check the new `.count` series on the active producer against pool-build counts
and confirm that Datadog indexes `configname`, `stage`, and `quantile`. For the producer's exported
rolling p50 (substitute `0.95` or `0.99` for p95 or p99 if configured):

```text
avg:base.reth_base_builder_native_payload_finalization_stage_seconds.quantile{configname:mainnet-ha-sequencer-1-k8s,quantile:0.5} by {stage}
```

Use the active producer only, not validators or shadow builders. These are exporter-window
quantiles, not percentiles over all blocks in a day. Compute a block-weighted mean from the
stage's `.sum` divided by its `.count` over the same interval.
