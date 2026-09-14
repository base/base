# `base-batcher-cli`

Arguments and startup for `base batcher`.

Submits L2 batch data to the L1 DA layer. Wraps `base-batcher-service` with
CLI argument parsing and signal handling.

Logging and metrics use the shared `base` flags and `BASE_NODE_LOG_*` /
`BASE_NODE_METRICS_*` environment variables.

L1 RPC uses `--l1-rpc-url` / `BASE_NODE_L1_ETH_RPC`. Top-level `--chain` /
`BASE_CHAIN` selection is rejected. Chain details and the batch inbox are fetched
from the rollup RPC.

## Configuration

`--compressed-size-target` optionally closes a channel after an accepted batch
reaches the target. `--max-blobs-per-tx` caps blob packing per L1 transaction,
while `--max-calldata-size-bytes` caps calldata transactions. `--brotli-quality`
selects Brotli quality `0..=11` (default 10). `--data-availability-type`
selects blobs or calldata; `--max-channel-duration` and `--sub-safety-margin`
control channel lifetime. For calldata configurations,
`--no-force-blobs-when-throttling` disables the throttle-driven blob override.
The corresponding environment variables use the `BASE_BATCHER_` prefix.

## Shadow mode

`base batcher` normally reads `batch_inbox_address` from the rollup RPC's
`optimism_rollupConfig` response and submits DA transactions to that canonical
inbox.

Shadow deployments may set `--shadow-mode` together with
`--dangerously-override-batch-inbox-address` to submit to a non-canonical inbox.
The flags must be set together so production deployments cannot redirect DA by
accident. Shadow deployments can use either the local `--private-key` signer or
the production remote-signer path with `--signer-endpoint` and
`--signer-address`.

This override only changes where the batcher writes. It does not make a stock
`base-consensus` verifier derive those batches: derivation filters DA by both
`RollupConfig.batch_inbox_address` and the current `SystemConfig.batcher_address`.
A shadow verifier must therefore use accepted inbox and signer inputs that match
the shadow submissions. Do not add permanent production consensus bypass logic
just to support this rollout.

Shadow deployments use an isolated parity validator to derive their submitted
data. The batcher compares its derived L2 block hashes with the canonical
sequencer through `--parity-validator-l2-rpc-url`.
