# base-execution-eip8130

Native EIP-8130 (Account Abstraction by Account Configuration) validation helpers.

This crate owns the full reusable EIP-8130 validation pipeline that previously lived
across several small crates:

- stateless authenticator dispatch (`AuthenticatorDispatch`),
- `AccountConfiguration` storage reads (`AccountConfigurationStorage`),
- stateful actor authorization (`ActorAuthorizer`),
- transaction sender/payer and config-change authorization (`ActorTxVerifier`,
  `ConfigChangeAuthorizer`), and
- 2D nonce validation (`NonceValidator`).

The split is now internal module structure instead of independent workspace crates.

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

## Validation Layers

The crate keeps the protocol stages explicit while avoiding crate sprawl:

- **Dispatch** verifies canonical authenticator blobs and resolves actor ids.
- **State** reads `AccountConfiguration` storage directly, without EVM calls.
- **Authorize** binds resolved actors to account config, expiry, scope, and policy.
- **Transaction auth** applies sender, payer, and config-change operation gates.
- **Nonce validation** checks protocol, 2D-channel, and nonce-free replay state.
