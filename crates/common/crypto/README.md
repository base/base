# base-common-crypto

Shared elliptic-curve signature primitives used across the Base protocol.

This crate is the single home for the raw signature operations that the EIP-8130
enshrined authenticators rely on, so the heavy curve logic and — critically — the
**malleability policy** live in exactly one audited place:

- [`Secp256k1::recover`] — recovers the signer of a 32-byte prehash from a
  65-byte `r || s || v` signature, requiring `v in {27, 28}` and enforcing
  **EIP-2 low-`s`**.
- [`Secp256r1::verify_prehash`] — verifies a P-256 signature `(r, s)` over a
  prehash for the public key `(x, y)`, enforcing low-`s` to match OpenZeppelin
  `P256.verify`.

## Why not call the precompiles?

Both operations bottom out in the same `k256` / `p256` crates the EVM precompiles
use, but the precompile *wrappers* deliberately disagree on malleability:

- the `ecrecover` precompile (`0x01`) **normalizes** a high-`s` signature and
  accepts it;
- the RIP-7212 `P256VERIFY` precompile does **not** enforce low-`s`.

The EIP-8130 authenticators must instead **reject** malleable (high-`s`)
signatures, both to preserve transaction-hash non-malleability and to stay
byte-parity with the deployed `AccountConfiguration` / OpenZeppelin-based
authenticator contracts. This crate provides that policy as a reusable primitive
rather than re-deriving it at each call site.
