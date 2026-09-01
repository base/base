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
- **Protocol logs** inject `ActorAuthorized` / `ActorRevoked` / `AccountCreated` /
  `DelegationApplied` into the journal from the enshrined apply path
  (`AccountConfigurationEvents`), matching the `IAccountConfiguration` event ABIs
  so indexers can enumerate actors from 8130 transaction receipts.

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
| `nonce_key` | nonce-free `13,000`; otherwise first-use `22,100` / existing `5,000` (a cold SLOAD plus an SSTORE set or reset) |
| `bytecode` | per create entry: `32,000 + 200 · code_len` |
| `account_changes` | per create entry: one fresh packed `account_state` write plus one fresh `actor_config` slot write per initial actor; policy actors set `policy_commitment`/`policy_manager`, while ungated actors pay the cold zero-to-zero touches that preserve access warming; per config-change entry: a packed `account_state` write covering the sequence advance and lock read — the first access to that slot in the transaction (create bootstrap or first config change) is a cold zero-to-nonzero write, later same-account bumps are only a warm SLOAD + dirty SSTORE (`200`, the slot was already modified earlier in the transaction) — its `auth` cost, and each mutated actor/policy slot; revokes conservatively price all three actor/policy resets, except each revoke slot execution resolves to be an empty zero-to-zero touch is discounted by the reset-vs-cold-noop delta (an inline secp256k1 self revoke discounts its always-empty `actor_config` slot plus each policy slot — `manager`/`commitment` — whose stored value is zero, so three empty slots when ungated and one to three when policy-gated, since the EIP permits a gated actor to carry a zero manager and/or commitment); a self-actor change adds no separate bump — its inline-self write is already covered by the config-change `account_state` cost and its `actor_config(self)` home by the per-change slot cost; per delegation entry: the `4,600` indicator deposit |
| `auto_delegation` | `4,600` when a code-less `sender` EOA is auto-delegated to `DEFAULT_ACCOUNT` |
| `sender_auth` / `payer_auth` | authenticator execution gas + one cold config/state SLOAD, plus one cold `policy_manager` SLOAD when the resolved actor has `SCOPE_POLICY`; `payer_auth` is `0` for self-pay |

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
`payer_auth_cost` is `0`. `validate_gas_and_tip` extends that check with a
declared coinbase tip: self-pay must cover gas and tip from one balance, while
sponsored payment requires the payer to cover gas and the sender to cover the
tip.

The gas and fee layer is pure accounting: it reads no state and runs no EVM. The
state-derived inputs (whether the nonce channel is first-use, whether the sender
is auto-delegated, and whether sender/payer actors are policy-gated) are supplied
by the caller via `IntrinsicGasInput`. It does
not advance nonces, debit balances, or execute calls; cold/warm and set/reset
refinements that depend on intra-transaction access order are finalized by the
execution metering layer.
