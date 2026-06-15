# base-execution-eip8130-tx

Transaction-level actor authorization for EIP-8130 — the first transaction-aware
layer of the validation pipeline, shared by mempool admission and block
inclusion. It turns the single-actor [`ActorAuthorizer`](../eip8130-authorize)
into "authorize *this transaction's* actors".

For an [`Eip8130Signed`] it resolves and authorizes the two transaction actors
against the [`AccountConfiguration`](../eip8130-state) storage, then enforces the
per-operation **scope** gate:

1. **Sender** — resolve the sender account (`tx.sender` for a configured account,
   or the checked-recovered signer on the EOA path), authorize its `sender_auth`
   against `sender_signature_hash`, and require **`SCOPE_SENDER`**.
2. **Payer** — when `tx.payer` is set, authorize its `payer_auth` against
   `payer_signature_hash(resolved_sender)` and require **`SCOPE_PAYER`**. A `None`
   payer means the sender pays; no payer authorization is performed.

An actor with `scope == 0` (unrestricted) satisfies every context; otherwise the
relevant scope bit must be set, matching `AccountConfiguration`'s
`scope == 0 || scope & REQUIRED != 0` rule.

## Sender auth formats

The sender auth blob differs by path, mirroring the wire format:

- **Configured account** (`tx.sender == Some`): `sender_auth` is already
  `authenticator(20) || data` and is passed through unchanged.
- **EOA** (`tx.sender == None`): `sender_auth` is a bare 65-byte `r‖s‖v`
  signature. The sender is recovered with the **checked** (EIP-2) recovery, and
  an `address(0)` authenticator prefix is synthesized so the unified
  `authenticate_actor` runs the implicit-EOA path (self-slot empty + recovered ==
  account, unrestricted owner).

## Scope (what this is and is not)

This layer covers **sender + payer** authorization and scope gating only. It does
**not** validate account changes (the `SCOPE_CONFIG` owner-changes path, layered
next), 2D nonces, gas/payment amounts, account locking, or call execution — those
consume the [`TxActors`] this returns.

[`Eip8130Signed`]: base_common_consensus::Eip8130Signed
