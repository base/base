# base-execution-eip8130-rpc

RPC-layer helpers for EIP-8130 2D nonce reads.

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
