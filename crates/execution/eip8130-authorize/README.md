# base-execution-eip8130-authorize

The stateful **Authorize** step of the EIP-8130 validation flow (step 3), shared
by mempool admission and block inclusion. It turns the stateless
[`DispatchOutcome`](../eip8130) of the **Authenticate** step into a resolved,
*authorized* actor by reading the [`AccountConfiguration`](../eip8130-state)
storage.

This is the native mirror of `AccountConfiguration.authenticateActor` /
`_authenticate`. Given an account, a signing `hash`, and an auth blob
(`authenticator(20) || data`), it:

1. routes the authenticator (implicit-EOA `address(0)`, the native ecrecover
   sentinel `address(1)`, P-256, `WebAuthn`, or delegate) through the enshrined
   stateless dispatch to resolve the `actorId`;
2. looks up `actor_config[actorId][account]`, requiring the stored authenticator
   to match the one that signed and the actor not to be expired; and
3. returns the actor's **authorization surface** — `scope`, `policyType`, and the
   resolved `policyTarget` (policy *manager*, never the signed commitment) — so a
   caller can make a full authorization decision without re-deriving the actor.

## Paths

| authenticator        | resolution                                                            |
| -------------------- | --------------------------------------------------------------------- |
| `address(0)` implicit| ecrecover; requires the self-actor slot empty and `recovered == account`; always an unrestricted owner |
| `address(1)` ecrecover | ecrecover → `actorId = bytes20(recovered)`; bound to `ECRECOVER_AUTHENTICATOR` |
| P-256 / `WebAuthn`   | dispatch → `actorId = keccak256(x‖y)`; bound to its authenticator      |
| delegate             | two-step: authorize the nested actor against the delegated account, then the outer `bytes20(delegate)` actor against the originating account under the delegate authenticator |

## Scope (what this is and is not)

This layer resolves and authorizes a **single actor** against the account's
config: it performs the `actor_config` / `policy_manager` reads, the
authenticator-binding check, the expiry check, and the implicit-EOA rule. It
does **not** enforce the transaction-level scope/operation gating (`SCOPE_SENDER`
for the sender, `SCOPE_PAYER` for the payer, `SCOPE_CONFIG` for actor changes),
account locking, nonces, or gas — those are the consuming validator's
responsibility and layer on top of the returned surface.

### Parity & limitations

The native authorize logic MUST produce results identical to
`AccountConfiguration` (see the dispatch crate's parity note). One deliberate
limitation inherited from dispatch: a delegate whose *nested* authenticator is
the implicit-EOA (`address(0)`) is **not** supported, because dispatch rejects a
non-canonical nested authenticator before this layer is reached. Delegating to an
explicit ecrecover (`address(1)`) actor on the delegated account is supported.
