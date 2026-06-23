# `base-batcher`

The Base Batcher binary.

Submits L2 batch data to the L1 DA layer. Wraps `base-batcher-service` with
CLI argument parsing and signal handling.

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

Shadow rollout parity is split into two checks: DA parity compares decoded
canonical and shadow submissions, and derived block parity compares the L2 block
hashes reported by an isolated parity-validator RPC.
