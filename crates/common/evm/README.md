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

The mechanism is one hook: `BaseHandler::refund` spends the transaction's remaining gas and drops
its refund counter when the feature is active, fee charging is enabled, and the transaction is not
a deposit. Everything downstream of revm's result (the EIP-7623 floor, `reimburse_caller`, the
operator-fee refund, `reward_beneficiary`, `ResultGas`) then observes `gas_used == gas_limit`
without further special cases, so the block executor, the payload builder,
and receipts agree by construction.

| Component | Feature off | Feature on |
|---|---|---|
| Sender debit | `gas_used * effective_gas_price`; unused gas, EIP-3529 refunds and the operator-fee surplus are reimbursed | `gas_limit * effective_gas_price`; nothing is reimbursed |
| Coinbase tip, `BaseFeeVault`, `OperatorFeeVault` | on `gas_used` | on `gas_limit` |
| Receipt `gasUsed`, `cumulativeGasUsed`, header `gas_used` | metered | `gas_limit`, so `gasUsed * effectiveGasPrice` is the fee paid and receipts still sum to the header |
| `eth_call`, `eth_estimateGas` (`disable_fee_charge`) | metered | metered: simulation bills nothing, so estimation still finds the gas a call needs |
| `eth_simulateV1`, `debug_trace*` / `trace_*` replays (fee charging left on by reth) | metered | the reserved gas, with per-opcode trace costs still metered; whether to exempt these paths is decided separately (BASE-1434) |
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
once. Before the feature the estimate includes `payer_floor` and settlement bills metered gas.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
