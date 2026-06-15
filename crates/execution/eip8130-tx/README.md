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

## Config-change authorization

[`ConfigChangeAuthorizer`] authorizes an account-configuration change (a
`ConfigChange` entry in `account_changes`) against the account's config — the
native mirror of `AccountConfiguration.applySignedActorChanges`'s authorization
tail. For one entry it:

1. rejects a **locked** account (`onlyUnlocked`);
2. enforces the **chain binding** and selects the sequence channel
   (`chain_id == 0` → multichain, else local);
3. requires the entry's `sequence` to equal the account's current channel
   sequence (the value the contract reads from state and the signer signs over);
4. reconstructs the **`SignedActorChanges`** digest (byte-identical to
   `_computeSignedActorChangesDigest`), authorizes the entry's `auth` through the
   [`ActorAuthorizer`], and requires **`SCOPE_CONFIG`**.

It authorizes a single entry against the account's *current* on-chain sequence;
ordering/sequence advancement across multiple same-channel entries in one
transaction, and **applying** the changes (decoding `data`, mutating
`actor_config`), are layered on top.

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

This layer covers **sender + payer** authorization (via [`ActorTxVerifier`]) and
single-entry **config-change** authorization (via [`ConfigChangeAuthorizer`]),
each with its scope gate, plus the config-change lock/chain/sequence checks. It
does **not** apply account changes, simulate cross-entry sequencing, or validate
2D nonces, gas/payment amounts, or call execution — those consume the
[`TxActors`] / [`ResolvedActor`] these return.

[`Eip8130Signed`]: base_common_consensus::Eip8130Signed
[`ResolvedActor`]: base_execution_eip8130_authorize::ResolvedActor
