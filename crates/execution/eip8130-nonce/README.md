# base-execution-eip8130-nonce

2D nonce validation for EIP-8130 — the stateful nonce step of the validation
pipeline, shared by mempool admission and block inclusion. It consumes the
resolved sender account (from [`base-execution-eip8130-tx`](../eip8130-tx)) and
checks a transaction's `(nonce_key, nonce_sequence)` against the live nonce
state.

[`NonceValidator::validate`] resolves the channel implied by `nonce_key` and
compares the transaction's `nonce_sequence` to that channel's current value:

- **Protocol nonce** (`nonce_key == 0`): the channel value is the account's
  basic protocol nonce, held in account state rather than the nonce manager, so
  it is supplied by the caller as `protocol_nonce`.
- **2D channel** (`0 < nonce_key < NONCE_KEY_MAX`): the channel value is read
  from the [`NonceManager`](base_common_precompiles::NonceManagerStorage)
  precompile via `get_nonce(account, nonce_key)`. Independent channels let one
  account keep many independently-ordered transactions in flight.
- **Nonce-free** (`nonce_key == NONCE_KEY_MAX`): there is no sequence channel.
  Replay protection comes from the nonce manager's expiring-nonce set, keyed by
  the signature-invariant replay hash `keccak256(account ‖ sender_signature_hash)`
  so re-signed fee-payer variants of one logical transaction collapse to a single
  entry.

## Modes

The same comparison serves both consumers, differing only in how a sequence
*ahead* of the channel is treated:

- [`NonceMode::Inclusion`] — the sequence must equal the channel nonce. A higher
  sequence is a gap that cannot execute now ([`NonceError::TooHigh`]).
- [`NonceMode::Pool`] — a higher sequence is admissible and **buffered**
  ([`NonceStatus::Buffered`]) until its predecessors arrive, matching mempool
  semantics for gapped transactions.

A sequence *below* the channel nonce is always stale ([`NonceError::TooLow`]).

## Scope (what this is and is not)

This layer is a read-only check: it reads the channel nonce (or the
expiring-nonce set) and decides admissibility. It does **not** advance nonces,
record the expiring-nonce replay hash, or validate the structural nonce-free
rules (`nonce_sequence == 0`, the `expiry` window) — those are enforced by
`Eip8130Signed::validate_timestamp` and applied by the execution layer that
calls `increment_nonce` / `check_and_mark_expiring_nonce`.
