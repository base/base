# base-execution-eip8130-validate

Transaction-authorization orchestrator for EIP-8130, shared by mempool admission
and block inclusion. It composes the per-stage authorization primitives into one
verdict over a signed transaction: the final sender (and payer) signatures plus
the transaction's ordered set of account-configuration changes.

## What it does

[`TransactionAuthorizer::authorize`] runs the stateful authorization flow against
an `AccountConfigurationStorage` view:

1. **Final transaction signatures** — resolves and scope-gates the sender
   (`SCOPE_SENDER`) and, when sponsored, the payer (`SCOPE_PAYER`) via the
   `eip8130-tx` `ActorTxVerifier`. The sender signature commits to the whole
   transaction body, so it authorizes the envelope — including the `calls`,
   `metadata`, and the presence of every account change.
2. **Account-configuration changes** — walks `account_changes` in order and
   authorizes each `ConfigChange` against the sender account with its own
   `SignedActorChanges` signature (`SCOPE_CONFIG`), the account lock, and the
   chain binding.

`Create` and `Delegation` entries carry no independent signature at this layer:
they are authorized by the sender signature that commits to the body (and, for
`Create`, by the deterministic deploy-address derivation enforced at execution).

### Sequence advancement

A config-change channel (multichain `chain_id == 0`, or the local chain)
advances by one per applied entry. `ConfigChangeAuthorizer::authorize` alone only
checks an entry against the channel's *current* on-chain sequence, so a
transaction carrying several same-channel entries would otherwise fail on the
second. The orchestrator reads each channel's base sequence once and validates
entries against the running value (`base`, `base + 1`, …) in transaction order,
using `ConfigChangeAuthorizer::authorize_at_sequence`.

## Scope (what this is not)

Authorization only: it reads state but never mutates it (no `actor_config`
writes, no sequence or nonce advancement, no balance debits). Nonce validation,
intrinsic-gas accounting, and fee/balance checks are separate stages
(`eip8130-nonce`, `eip8130-gas`); composing them into a single end-to-end
transaction verdict is layered on top. Structural and expiry-window validation of
the transaction body is performed upstream in the consensus layer.
