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
selects Brotli quality `0..=11` (default 9). `--data-availability-type`
selects blobs or calldata; `--max-channel-duration` and `--sub-safety-margin`
control channel lifetime. For calldata configurations,
`--no-force-blobs-when-throttling` disables the throttle-driven blob override.
The corresponding environment variables use the `BASE_BATCHER_` prefix.

## Shadow mode

`base batcher` reads `batch_inbox_address` from the rollup RPC's
`optimism_rollupConfig` response and posts DA transactions to that inbox. It
follows the derivation of the same node, which derives its chain from that
inbox.

Outside shadow mode, `base batcher` refuses to start unless its signer is the
current `SystemConfig` batcher address, since derivation would ignore every
batch it posts.

A shadow deployment posts to a non-canonical inbox that an isolated parity
validator derives its chain from. `--rollup-rpc-url` points at the validator's
rollup node, whose rollup config names that inbox.
`--dangerously-override-batch-inbox-address` names the shadow inbox, and the
batcher refuses to start if the batch inbox of the rollup node's config is
another one. The parity validator must accept the shadow inbox and signer,
since derivation filters batches by both. `--parity-validator-l2-rpc-url`
points at the validator's L2 RPC, whose derived blocks the batcher compares
with the canonical sequencer's. The batcher refuses to start with only some of
`--shadow-mode`, `--dangerously-override-batch-inbox-address` and
`--parity-validator-l2-rpc-url`. `--l2-rpc-url` points at the canonical
sequencer, to which the throttle would push its DA limits, so the batcher
refuses to start in shadow mode without `--no-throttle`. Shadow deployments can
use either the local `--private-key` signer or the production remote-signer
path with `--signer-endpoint` and `--signer-address`.
