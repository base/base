# base-execution-eip8130

Native EIP-8130 (Account Abstraction by Account Configuration) validation helpers.

This crate owns the full reusable EIP-8130 validation pipeline that previously lived
across several small crates:

- stateless authenticator dispatch (`AuthenticatorDispatch`),
- `AccountConfiguration` storage reads (`AccountConfigurationStorage`),
- stateful actor authorization (`ActorAuthorizer`),
- transaction sender/payer and config-change authorization (`ActorTxVerifier`,
  `ConfigChangeAuthorizer`),
- 2D nonce validation (`NonceValidator`), and
- intrinsic gas accounting and fee/balance validation (`IntrinsicGas`,
  `Eip8130GasSchedule`, `FeeCheck`).

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
- **Gas and fees** compute the EIP-8130 intrinsic gas and validate the fee caps
  and the payer's balance (see below).
- **Orchestration** composes the final sender/payer signatures with the
  transaction's ordered account-configuration changes — advancing each channel
  sequence per applied entry — into one authorization verdict (`TransactionAuthorizer`),
  shared by mempool admission and block inclusion. It reads state but never
  mutates it; nonce, gas, and fee/balance checks remain separate stages.

## Intrinsic gas

`IntrinsicGas::compute` returns the per-component breakdown from the EIP-8130
formula:

```text
intrinsic_gas = AA_BASE_COST + tx_payload_cost + nonce_key_cost + bytecode_cost
              + account_changes_cost + auto_delegation_cost
              + sender_auth_cost + payer_auth_cost
```

| Component | Source |
|---|---|
| `base` | `AA_BASE_COST` (15,000) |
| `payload` | EIP-2028 data-availability cost (16/non-zero, 4/zero byte) over the caller-supplied EIP-2718 serialization of the signed transaction |
| `nonce_key` | nonce-free `14,000`; otherwise first-use `22,100` / existing `5,000` (a cold SLOAD plus an SSTORE set or reset) |
| `bytecode` | per create entry: `32,000 + 200 · code_len` |
| `account_changes` | per create entry: one fresh `actor_config` slot write per initial actor (unrestricted owner, no policy slots); per config-change entry: its `auth` cost plus each mutated actor slot (`actor_config`, plus `policy_commitment`/`policy_manager` when the authorize carries a policy), plus a worst-case dual-home bump (`22,100`) for a change targeting the account's own self-actor (whose config is inline in the account-state slot and is mutually exclusive with `actor_config(self)`); per delegation entry: the `4,600` indicator deposit |
| `auto_delegation` | `4,600` when a code-less `sender` EOA is auto-delegated to `DEFAULT_ACCOUNT` |
| `sender_auth` / `payer_auth` | authenticator execution gas + one cold `authorize` SLOAD: a bare signature reads the account-state slot carrying the inline self config, and every resolved authenticator (explicit `K1_AUTHENTICATOR`, P-256, `WebAuthn`) reads one slot — the inline self-config model resolves any k1 self in a single read; `payer_auth` is `0` for self-pay |

`sender_intrinsic` excludes `payer_auth` (payer authentication is metered on top
of `gas_limit`), so `execution_gas_available(gas_limit) = gas_limit -
sender_intrinsic`.

### Authenticator execution gas

The EIP lets a chain enshrine the canonical authenticators and charge a fixed
gas per authenticator. `Eip8130GasSchedule` pins these to the EVM precompile
costs Base already uses:

| Authenticator | execution gas | basis |
|---|---|---|
| secp256k1 (`K1_AUTHENTICATOR` sentinel, EOA path) | 3,000 | `ECRECOVER` precompile |
| P-256 | 6,900 | EIP-7951 `P256VERIFY` precompile |
| `WebAuthn` | 6,900 | P-256 verify + SHA-256 + `clientDataJSON` handling |
| delegate (depth-1) | `2,100 + nested` | extra cold `actor_config` SLOAD on the delegate account + the nested authenticator's execution |

The EIP-8130 gas schedule is a recommendation at the current point in time;
chains may implement a different schedule. A `#[cfg(test)]` drift tripwire pins
the EVM gas primitives to revm's canonical constants so an upstream repricing is
caught here rather than silently diverging.

## Fees and balance

`FeeCheck` validates the EIP-1559 fee caps against the block base fee and bounds
the payer's worst-case ETH debit at `(gas_limit + payer_auth_cost) ·
max_fee_per_gas` — `payer_auth_cost` is added because payer authentication is
charged on top of `gas_limit`. For self-pay the payer is the sender and
`payer_auth_cost` is `0`.

The gas and fee layer is pure accounting: it reads no state and runs no EVM. The
state-derived inputs (whether the nonce channel is first-use, whether the sender
is auto-delegated) are supplied by the caller via `IntrinsicGasInput`. It does
not advance nonces, debit balances, or execute calls; cold/warm and set/reset
refinements that depend on intra-transaction access order are finalized by the
execution metering layer.
