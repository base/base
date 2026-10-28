# `base-common-evm`

EVM implementation.

## Overview

Provides Base-specific EVM execution support. Maps upgrade activation timestamps to revm
`SpecId` values, and exposes `BaseEvm`, `BaseEvmFactory`, `BaseBlockExecutor`, and
`BaseBlockExecutorFactory` for executing blocks with the correct gas rules and precompile sets for
each upgrade. Also provides `AlloyReceiptBuilder` and `BaseReceiptBuilder` for constructing Base
receipts and
`ensure_create2_deployer` for Canyon upgrade compatibility.

## Gas-limit fee billing (Zenith proof of concept)

From the genesis-only Zenith testing gate, the EVM charges a non-deposit transaction for its full
declared `gas_limit` instead of the gas it consumed. The intent is to make a transaction's fee
knowable before it executes, which lets a block builder order and admit transactions by the ETH
they will actually pay (`gas_limit * priority_fee_per_gas`, rather than the per-gas tip alone) and
is a prerequisite for sequencing transactions before executing them.

`BaseSpecId::is_gas_limit_billing_enabled` is the single feature predicate. When it is active:

| Component | Before Zenith | From Zenith |
|---|---|---|
| Sender debit (`BaseHandler::reimburse_caller`) | `gas_used * effective_gas_price`; unused gas, EIP-3529 refunds and the operator-fee surplus are reimbursed | `gas_limit * effective_gas_price`; nothing is reimbursed |
| Coinbase tip, `BaseFeeVault` burn, `OperatorFeeVault` (`BaseHandler::reward_beneficiary`) | on `gas_used` | on `gas_limit` |
| EIP-8130 payer (`Eip8130Executor::settle_fees`) | `billable_gas` (sender intrinsic + calls, less refunds, plus payer auth) | the full pre-charge, `gas_limit + payer_auth` |
| Block gas accounting (`BaseBlockExecutor`, and the flashblocks builder's own receipts) | `header.gas_used` and receipt `cumulative_gas_used` accrue `gas_used` | accrue the gas each transaction reserved (`gas_limit`, or `gas_limit + payer_auth` for EIP-8130), so a receipt's `gasUsed * effectiveGasPrice` is the fee paid and a block's gas fullness follows from gas limits alone |
| L1 data fee | unchanged | unchanged |
| Deposit transactions | no fee | no fee, actual gas accounted |
| `ExecutionResult::gas_used` (traces, resource metering, `eth_estimateGas`, `eth_call`) | actual consumption | actual consumption; fee-charge-disabled simulation bills nothing |

Because `eth_estimateGas` keeps converging on the real requirement, the estimate becomes the
quantity that determines the fee: over-provisioning a gas limit now costs real ETH.

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

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
