# base-execution-eip8130

EIP-8130 (Account Abstraction by Account Configuration) authenticator **dispatch**.

This crate implements step 2 ("Authenticate") of the EIP-8130 validation flow: given a
signing `hash` and an authentication blob (`authenticator || data`), it routes to the
named authenticator, verifies the signature, and returns the resolved `actorId`.

## Enshrined, not a precompile

The canonical authenticators (P-256, `WebAuthn`, Delegate; native secp256k1 ecrecover for
k1) are **enshrined** here as native Rust implementations keyed by their canonical
CREATE2 addresses (from `base-common-consensus::Eip8130Contracts`). This is the
protocol's own fast-path for authenticating AA transactions during validation and block
execution; the EIP explicitly permits enshrining canonical authenticators at a fixed gas
cost provided results are identical to the deployed contract.

This is **not** an EVM precompile and does **not** shadow the authenticator addresses:
ordinary EVM `CALL`/`STATICCALL` to those addresses still hits the real deployed
contract bytecode (e.g. `AccountConfiguration.verifySignature()`, `applySignedActorChanges()`
on non-8130 chains, wallet code). The native code here is invoked only by the protocol.

## Parity is required

Because the enshrined path and the EVM path can authenticate the same actor, the native
implementation MUST produce byte-identical `actorId` results to the deployed authenticator
contracts. The enshrined logic is pinned to a specific contract version via the
`init_code_hash` constants in `Eip8130Contracts`; a contract bytecode change shifts the
canonical address (caught by the registry drift test) and requires re-pinning the address
and re-validating parity here. A differential test against the deployed contracts (via the
EVM) is a planned follow-up.

## Scope

This crate is **stateless / pure**: it performs no storage reads and runs no EVM. The
stateful "Authorize" step (reading `actor_config`, the implicit-EOA rule, scope/expiry,
and the delegate authenticator's nested-actor authorization in the delegated account's
SIGNATURE context) is layered on top in a later stage. For the delegate authenticator,
dispatch verifies the nested signature and surfaces the nested actor as a
[`DispatchOutcome::Delegated`] obligation for that authorize stage to discharge.
