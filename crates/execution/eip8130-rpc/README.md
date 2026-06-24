# base-execution-eip8130-rpc

RPC-layer helpers for EIP-8130: 2D nonce reads and gas estimation.

## `ChannelNonceReader`

Exposes [`ChannelNonceReader`], which resolves a `(address, nonce_key)` channel
nonce against a state snapshot at a given block, optionally honoring
state overrides (e.g. flashblocks pending state).

Behavior by `nonce_key`:

- `nonce_key == 0`: delegates to the standard protocol nonce path
  (`EthState::transaction_count`), since the protocol nonce lives in
  `account.nonce`, not in the Nonce Manager precompile's storage.
- `nonce_key == NONCE_KEY_MAX`: returns an `INVALID_PARAMS` RPC error.
  This sentinel value selects the expiring-nonce / nonce-free channel,
  which has no per-channel counter — replay protection there relies on
  `expiry` instead.
- otherwise: derives the precompile storage slot via
  `NonceManagerStorage::nonce_slot`, consults any provided state overrides
  for that slot, falls back to a raw `StateProvider::storage` lookup
  against the requested block, and decodes the resulting u64 from the
  slot's low 8 bytes.

This avoids the cost of an `eth_call` to the precompile's `getNonce` for
the common `nonce_key != 0` case while keeping layout ownership inside
`base-common-precompiles` (via `NonceManagerStorage::nonce_slot`).

## `Eip8130GasEstimator`

Exposes [`Eip8130GasEstimator`], which estimates gas for an `eth_estimateGas`
request carrying EIP-8130 fields (account changes, calls, `nonce_key`, expiry,
metadata). It builds an unsigned simulation transaction — a stub k1
authentication blob stands in for the bare-signature EOA wire form so the
intrinsic schedule prices authentication gas from its shape — and runs a single
read-only `base_common_evm::Eip8130Executor::simulate` against the block state.

Because the EIP-8130 pipeline charges a deterministic, signature-independent
amount (intrinsic + phased-call gas, less the EIP-3529-capped refund, plus payer
authentication), one simulation yields the exact estimate; no gas-limit binary
search is needed. Plain (non-8130) requests fall through to the standard reth
estimator unchanged.

Both helpers are fork-agnostic: callers gate on the Cobalt hard fork via
[`Eip8130CobaltGate`] before invoking them.
