# `base-common-evm`

EVM implementation.

## Overview

Provides Base-specific EVM execution support. Maps upgrade activation timestamps to revm
`SpecId` values, and exposes `BaseEvm`, `BaseEvmFactory`, `BaseBlockExecutor`, and
`BaseBlockExecutorFactory` for executing blocks with the correct gas rules and precompile sets for
each upgrade. Also provides `AlloyReceiptBuilder` and `BaseReceiptBuilder` for constructing Base
receipts and
`ensure_create2_deployer` for Canyon upgrade compatibility.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-evm = { workspace = true }
```

```rust,ignore
use base_common_evm::{BaseEvmFactory, BasePrecompiles, BaseSpecId, BaseUpgrade};

let factory = BaseEvmFactory::default();
let precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl)).install();
```

## Gas-limit billing

Under the `GasLimitBilling` feature (gas-limit billing, activated by the Everest upgrade), the EVM
charges a non-deposit transaction for its full declared `gas_limit` instead of the
gas it consumed, so a transaction's fee is fixed by public inputs before it executes and a
high-tip transaction that reverts early costs what the execution it was sized for would have cost.
The rationale, billing rule, implementation boundaries, alternatives, and design decisions are in
[`docs/guides/GAS_LIMIT_BILLING.md`](../../../docs/guides/GAS_LIMIT_BILLING.md).

The mechanism is one hook: `BaseHandler::post_execution` spends the transaction's remaining gas and
drops its refund counter, between revm's refund step and the result gas, when the feature is active,
fee charging is enabled, and the transaction is not a deposit. Everything downstream (`ResultGas`,
the EIP-7623 floor, `reimburse_caller`, the operator-fee refund, `reward_beneficiary`) then observes
`gas_used == gas_limit` without further special cases, so the block executor, the payload builder,
and receipts agree by construction.

Immediately before the hook, the handler records the gas the transaction metered (what it would
have been billed without the feature: spent less the capped EIP-3529 refund, raised to the EIP-7623
floor) in `MeteredGas`, a per-transaction thread-local slot like `Eip8130CommittedPhases` that the
caller of `transact` takes right after the call returns. `Eip8130Executor` publishes
`metered_gas_used` and the reserved `payer_floor` the same way. The payload builder reports it in its
`BUILDER_ACCEPTED` transaction event; nothing on the consensus path reads it.

| Component | Feature off | Feature on |
|---|---|---|
| Sender debit | `gas_used * effective_gas_price`; unused gas, EIP-3529 refunds and the operator-fee surplus are reimbursed | `gas_limit * effective_gas_price`; nothing is reimbursed |
| Coinbase tip, `BaseFeeVault`, `OperatorFeeVault` | on `gas_used` | on `gas_limit` |
| Receipt `gasUsed`, `cumulativeGasUsed`, header `gas_used` | metered | `gas_limit`, so `gasUsed * effectiveGasPrice` is the fee paid and receipts still sum to the header |
| `eth_call`, `eth_estimateGas`, `eth_simulateV1` with `validation=false` (`disable_fee_charge`) | metered | metered: simulation bills nothing, so estimation still finds the gas a call needs and a simulated call without `gas` does not consume the block's remaining gas |
| `eth_simulateV1` with `validation=true`, `debug_trace*` / `trace_*` replays of included transactions (fee charging on) | metered | the reserved gas, as a receipt reports it (`gas_limit + payer_floor` for a sponsored EIP-8130 transaction), with call frames and per-opcode trace costs still metered; `eth_simulateV1` rejects EIP-8130 requests |
| Deposit transactions | no fee, metered gas | no fee, metered gas |
| L1 data fee, DA footprint, intrinsic gas, pool ordering | unchanged | unchanged |

EIP-8130 transactions settle fees in `Eip8130Executor` rather than `BaseHandler`. Under the feature
it bills the reserved budget, `gas_limit + payer_floor`, instead of the gas the `calls` metered:
exactly what `prepay` pre-charged and what the block executor and builder reserve against the block
gas limit. The payer's surplus refund is zero; the coinbase tip, `BaseFeeVault` and
`OperatorFeeVault` amounts are computed on the reserved budget, and the result reports it as the gas
used. The `eth_estimateGas` estimate (`Eip8130Executor::simulate`) stays metered but stops folding
`payer_floor` into the returned limit, because settlement adds it on top of whatever limit the sender
declares: a sponsored transaction submitted at `gas_limit = estimate` settles `estimate + payer_floor`
once. EIP-8130 is only valid from Everest, which also activates the feature, so EIP-8130
transactions are always billed this way. With `disable_fee_charge` set, `Eip8130Executor::execute`
behaves exactly like the standard handler: signatures are still verified, the nonce is still
validated and the sender's nonce consumed, and the fee caps are still sanity-checked, but the
payer's balance is not checked, nothing is pre-charged and no settlement transfer runs, and the
result reports the metered gas net of the EIP-3529-capped refund. RPC requests for EIP-8130
(`eth_call`, `eth_estimateGas`) route through `Eip8130Executor::simulate`, which never settles
fees, and `eth_simulateV1` rejects them; honoring the flag in `execute` makes a fee-charge-disabled execution of a
*signed* EIP-8130 transaction behave the same way, so the two entry points cannot diverge.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
