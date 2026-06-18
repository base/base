# base-execution-eip8130-gas

Intrinsic gas accounting and fee/balance validation for EIP-8130, shared by
mempool admission and block inclusion. Given the signed transaction (and the few
state-derived hints the body alone cannot supply), it computes the EIP-8130
intrinsic gas and decides whether the gas payer can afford the transaction.

## Intrinsic gas

[`IntrinsicGas::compute`] returns the per-component breakdown from the EIP-8130
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
| `account_changes` | per create entry: one fresh `actor_config` slot write per initial actor (unrestricted owner, no policy slots); per config-change entry: its `auth` cost plus each mutated actor slot (`actor_config`, plus `policy_commitment`/`policy_manager` when the authorize carries a policy); per delegation entry: the `4,600` indicator deposit |
| `auto_delegation` | `4,600` when a code-less `sender` EOA is auto-delegated to `DEFAULT_ACCOUNT` |
| `sender_auth` / `payer_auth` | authenticator execution gas + one cold `actor_config` SLOAD; `payer_auth` is `0` for self-pay |

`sender_intrinsic` excludes `payer_auth` (payer authentication is metered on top
of `gas_limit`), so `execution_gas_available(gas_limit) = gas_limit -
sender_intrinsic`.

### Authenticator execution gas

The EIP lets a chain enshrine the canonical authenticators and charge a fixed
gas per authenticator. [`Eip8130GasSchedule`] pins these to the EVM precompile
costs Base already uses:

| Authenticator | execution gas | basis |
|---|---|---|
| secp256k1 (`K1_AUTHENTICATOR` sentinel, EOA path) | 3,000 | `ECRECOVER` precompile |
| P-256 | 6,900 | EIP-7951 `P256VERIFY` precompile |
| `WebAuthn` | 6,900 | P-256 verify + SHA-256 + `clientDataJSON` handling |
| delegate (depth-1) | `2,100 + nested` | extra cold `actor_config` SLOAD on the delegate account + the nested authenticator's execution |

## Fees and balance

[`FeeCheck`] validates the EIP-1559 fee caps against the block base fee and
bounds the payer's worst-case ETH debit at `(gas_limit + payer_auth_cost) ·
max_fee_per_gas` — `payer_auth_cost` is added because payer authentication is
charged on top of `gas_limit`. For self-pay the payer is the sender and
`payer_auth_cost` is `0`.

## Scope (what this is and is not)

Pure accounting and arithmetic: it reads no state and runs no EVM. The
state-derived inputs (whether the nonce channel is first-use, whether the sender
is auto-delegated) are supplied by the caller via [`IntrinsicGasInput`]. It does
not advance nonces, debit balances, or execute calls; cold/warm and set/reset
refinements that depend on intra-transaction access order are finalized by the
execution metering layer.
