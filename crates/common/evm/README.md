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
knowable before it executes: a block builder can then order and admit transactions by the ETH they
will actually pay (`gas_limit * priority_fee_per_gas`, rather than a per-gas tip that a reverting
or early-exiting transaction pays on almost no gas), and sequencing transactions before executing
them becomes possible. See also the parallel exploration in [base/base#6229], whose design doc
covers the builder's predicate-evaluation budget, alternatives, and rollout.

`BaseSpecId::is_gas_limit_billing_enabled` is the single feature predicate. The mechanism is one
hook: `BaseHandler::refund` spends the transaction's remaining gas and drops its refund counter
for non-deposit transactions when fee charging is enabled, so everything downstream of the revm
result observes `gas_used == gas_limit`. `Eip8130Executor::billable_gas` returns the reserved
`gas_limit + payer_auth` under the same gate. Nothing else special-cases the feature; the block
executor, both payload builders, flashblocks pending state, receipts, and RPC agree by
construction.

| Component | Before Zenith | From Zenith |
|---|---|---|
| Sender debit | `gas_used * effective_gas_price`; unused gas, EIP-3529 refunds and the operator-fee surplus are reimbursed | `gas_limit * effective_gas_price`; nothing is reimbursed |
| Coinbase tip, `BaseFeeVault` burn, `OperatorFeeVault` | on `gas_used` | on `gas_limit` |
| EIP-8130 payer (`Eip8130Executor::settle_fees`) | `billable_gas` (sender intrinsic + calls, less refunds, plus payer auth) | the reserved budget, `gas_limit + payer_auth` |
| EIP-8130 estimate (`Eip8130Executor::simulate`) | sender budget `+ payer_auth` | sender budget only, so `gas_limit = estimate` settles `estimate + payer_auth` once |
| Receipt `gasUsed`, `cumulativeGasUsed`, header `gas_used` | metered | the reserved gas, so `gasUsed * effectiveGasPrice` is the fee paid, receipts still sum to the header, and a block's gas fullness follows from gas limits alone |
| `ExecutionResult::gas_used` (traces, `eth_simulateV1` call results, builder metrics) | metered | the reserved gas; per-opcode trace costs stay metered |
| `eth_call`, `eth_estimateGas` (`disable_fee_charge`) | metered | metered: simulation bills nothing, so estimation still converges on the real requirement |
| Builder `--builder.max_gas_per_txn` | metered `gas_used` after execution | reserved `gas_limit + payer_auth` before execution (`ResourceLimits::tx_gas_limit`) |
| L1 data fee, DA footprint, intrinsic gas, validity rules, pool ordering | unchanged | unchanged |
| Deposit transactions | no fee, metered gas | no fee, metered gas |

Because `eth_estimateGas` keeps converging on the real requirement, the estimate becomes the
quantity that determines the fee: over-provisioning a gas limit now costs real ETH, and a limit
that turns out too low fails out-of-gas and still pays the limit, as it does today.

Open questions the proof of concept does not settle:

- EIP-3529 storage-clearing and EIP-7702 refunds have no economic effect: they cannot lower the
  limit a sender must declare, and nothing is reimbursed. Keeping the incentive would need
  opcode-level repricing rather than a refund.
- ERC-4337 bundlers pay for the bundle's `gas_limit` while the `EntryPoint` refunds measured
  `actualGas + preVerificationGas` per op, so the slack lands on the bundler until
  `preVerificationGas` or the `EntryPoint` accounting changes.
- `eth_simulateV1` reports reserved gas because reth never sets `disable_fee_charge` for it, and
  `Eip8130Executor::execute` settles fees regardless of that flag (pre-existing).
- Header `gas_used` now counts reserved gas, so the EIP-1559 base fee reacts to reserved demand
  and blocks hold at most `block_gas_limit` of declared gas; target and elasticity are unchanged.
- Wallet and tooling buffers on top of estimates (Foundry's 1.3x, viem/ethers/MetaMask defaults)
  become paid overhead; explorer "gas used / gas limit" displays read 100%.

[base/base#6229]: https://github.com/base/base/pull/6229

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
