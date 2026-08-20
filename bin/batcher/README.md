# `base-batcher`

The Base Batcher binary.

Submits L2 batch data to the L1 DA layer. Wraps `base-batcher-service` with
CLI argument parsing and signal handling.

## Configuration migration

Streaming channels no longer use `--target-frame-size` or
`BATCHER_TARGET_FRAME_SIZE`; blob frames use the protocol capacity directly.
Replace `--target-num-frames` with `--max-blobs-per-tx`, and replace
`--max-l1-tx-size-bytes` with `--max-calldata-size-bytes` for calldata DA.
`--brotli-quality` selects Brotli quality `0..=11` (default 10).
The corresponding environment variables use the new option names.

## Shadow mode

`base-batcher` normally reads `batch_inbox_address` from the rollup RPC's
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
