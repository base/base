# base-execution-base-tx-rpc

RPC-layer helpers for Base transactions: 2D nonce reads and gas estimation.

## `ChannelNonceReader`

Exposes [`ChannelNonceReader`], which resolves a `(address, nonce_key)` channel
nonce against a state snapshot at a given block, optionally honoring
state overrides layered on top of the canonical block.

Behavior by `nonce_key`:

- `nonce_key == 0`: delegates to the standard protocol nonce path
  (`EthState::transaction_count`), since the protocol nonce lives in
  `account.nonce`, not in the Nonce Manager precompile's storage.
- `nonce_key == NONCE_KEY_MAX`: returns an `INVALID_PARAMS` RPC error.
  This sentinel value selects the expiring-nonce / nonce-free channel,
  which has no per-channel counter — replay protection there relies on
  the `valid_before` validity-window bound instead.
- otherwise: derives the precompile storage slot via
  `NonceManagerStorage::nonce_slot`, consults any provided state overrides
  for that slot, falls back to a raw `StateProvider::storage` lookup
  against the requested block, and decodes the resulting u64 from the
  slot's low 8 bytes.

This avoids the cost of an `eth_call` to the precompile's `getNonce` for
the common `nonce_key != 0` case while keeping layout ownership inside
`base-common-precompiles` (via `NonceManagerStorage::nonce_slot`).

## `TxBaseGasEstimator`

Exposes [`TxBaseGasEstimator`], which estimates gas for an `eth_estimateGas`
request carrying Base transaction fields (the sender account via `sender` or `from`,
account changes, calls, `nonce_key`, `valid_after`/`valid_before`, and metadata).
It builds an unsigned simulation transaction — the caller's `sender_auth` blob
(`K1_AUTHENTICATOR || r || s || v` for a named sender, a bare signature for the
default EOA, or a stub when absent) — and runs a single read-only
`base_common_evm::TxBaseExecutor::simulate` against the block state.

Because the Base transaction pipeline charges a deterministic, signature-independent
amount (intrinsic + phased-call gas, less the EIP-3529-capped refund, plus payer
authentication), one simulation resolves the sender and prices intrinsic and
authentication gas once. Inside it, `simulate` binary-searches only the call
pool, re-dispatching the phased calls over reverted journal checkpoints, so the
returned limit covers the unrefunded call spend and EIP-150's 63/64 retention.
There is no outer gas-limit search like reth's standard estimator. Plain
(non-Base) requests fall through to the standard reth estimator unchanged.

The simulation applies the request's validity window at the simulated block's
time (honoring a `time` block override on `eth_call`) and applies its
delegation as inclusion would.

Both helpers are fork-agnostic: callers gate on the Everest hard fork via
[`TxBaseEverestGate`] before invoking them.
