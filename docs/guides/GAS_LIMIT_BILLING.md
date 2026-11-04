# Gas-Limit Billing

Gas-limit billing charges a Base transaction for the gas it reserves instead of the gas it consumes. It aligns payment with the priority a transaction receives from the builder, so high-tip transactions that terminate early cannot obtain cheap inclusion. This document defines the billing rule, its implementation boundaries, alternatives, and activation considerations.

## Problem

The builder evaluates and executes candidates in priority-fee order within a fixed flashblock time budget, including a separate validity-predicate evaluation budget. Ranking is by `tip` per gas, known at admission; payment is `tip * gas_used`, known after execution. A transaction can reserve enough gas for a costly execution path but terminate early when an opportunity is unavailable. A high tip then buys priority while the small amount of consumed gas keeps payment low. A flood of these transactions consumes evaluation and execution time, deferring other transactions.

Hypothetical example with a 1 gwei reference tip (illustrative values, not network measurements):

| | Arbitrage that executes | Reverting arb |
|---|---|---|
| `gas_limit` | 600,000 | 2,000,000 |
| `gas_used` | 500,000 | 25,000 |
| `tip` | 1 gwei | 20 gwei |
| Rank | second | first |
| Pays the sequencer, metered (`tip * gas_used`) | 0.0005 ETH | 0.0005 ETH |
| Pays the sequencer, gas-limit billing (`tip * gas_limit`) | 0.0006 ETH | 0.04 ETH |

In this example, gas-limit billing makes the reverting transaction's higher tip apply to its entire reservation. Searchers can reduce that cost by lowering the tip, tightening predicates so the transaction is not included unless the opportunity is available, or shrinking the limit.

What the rule does not fix: predicates that do not match are not included and pay nothing under either rule, and a tight-limit high-tip reverting transaction costs the same either way. Those are builder time-budget problems (pricing or capping predicate evaluation), out of scope here.

## Rule

For every user transaction in a block where gas-limit billing is active:

1. **Billable gas** `B = gas_limit` for standard transactions and `B = gas_limit + payer_floor` for EIP-8130 transactions: the quantity billed and reserved by the builder. This reservation is distinct from the pool's ranking denominator; static EIP-8130 coinbase-tip ordering continues to use `gas_limit`, not `B`. `payer_floor` is the payer-authentication gas (`IntrinsicGas::max_payer_auth_cost`: the `payer_auth` signature bytes priced at the EIP-7623 floor rate), metered on top of the sender's `gas_limit`; it is zero for self-paid transactions.
2. **Settlement.** The payer pays `B * effective_gas_price` plus the L1 data fee plus the operator fee on `B`. Unused gas is not reimbursed; EIP-3529 storage-clearing refunds, EIP-7702 authorization refunds, and stateful-precompile refunds do not reduce the fee. The EIP-7623 floor does not change the fee. Refund amounts depend on execution; applying them would make the final fee unknown when the builder orders transactions, defeating the design goal. The sequencer receives `B * priority_fee_per_gas`, the `BaseFeeVault` `B * basefee`, the `OperatorFeeVault` the operator fee on `B`. The L1 data fee is unchanged.
3. **Receipts and header.** Receipt `gasUsed = B`; `cumulativeGasUsed` and header `gasUsed` add `B` per user transaction, so `gasUsed * effectiveGasPrice` is still the execution fee and receipts still sum to the header.
4. **Deposits** are unchanged: no L2 execution fee, metered `gasUsed`.
5. **Unchanged:** ordering (`tip` per gas), validity rules, intrinsic gas, the EIP-7825 cap, the opcode gas schedule and out-of-gas semantics, L1 data fee, DA footprint, batching.

Execution is still metered internally; only what is settled and reported changes. No partial rate: any fraction `p < 1` leaves the discount, and the attack, in place at scale `1/p`.

| Property | Metered | Gas-limit billing |
|---|---|---|
| Ranking key | `tip` per gas | `tip` per gas (unchanged) |
| Sequencer revenue | `tip * gas_used`, known after execution | `tip * gas_limit`, known at admission |
| Cost of outranking a transaction | `tip` on the gas you end up using | `tip` on every unit you declared |
| Block fullness | metered sum; declared gas can exceed the block limit | reserved sum; at most `block_gas_limit` of declared gas |

## Alternatives considered

| Alternative | Why not |
|---|---|
| Rank by `tip * gas_limit`, pay metered | No longer a per-gas market, and the reverting arb still pays `tip * gas_used`. |
| Rank by realised payment `tip * gas_used` | Requires executing every candidate before ordering it, the cost this change removes. |
| Penalise reverts only | An early `STOP` is a successful transaction; the pattern does not need a revert. |
| Price predicate evaluation | Complementary, not a substitute: a passed-then-reverted arb stays cheap. |
| Pool heuristics on `gas_limit / gas_used` | Gameable with fresh addresses; per-sender state on the hot path; changes no payment. |
| Partial charge `gas_used + p * (gas_limit - gas_used)` | The gap shrinks by `1/p` but stays. |
| Fee on `gas_limit`, `gasUsed` stays metered | Breaks `fee = gasUsed * effectiveGasPrice`, which every explorer and wallet computes. |
| Header `gasUsed` from limits, receipts metered | Receipts no longer sum to the header. |
| Bill deposits by limit too | Deposits pay no L2 fee and are not tip-ordered; nothing to fix. |

## Implementation

`BaseFeature::GasLimitBilling` is a semantic feature in the hardfork feature registry ([`feature.rs`](../../crates/common/genesis/src/feature.rs)). The intended network activation gate is `BaseUpgrade::Everest`; the current registry assigns the feature to Zenith, the genesis-only development-testing gate. Network activation requires assigning it to Everest. Call sites check `spec.is_feature_enabled(BaseFeature::GasLimitBilling)`. Behavior is unchanged unless the feature is active. The hook lives in `crates/common/evm`; any replacement execution implementation must preserve the same settlement and reporting semantics.

**Standard transactions** ([`handler.rs`](../../crates/common/evm/src/handler.rs)). One hook in `BaseHandler::refund`, after revm's refund step:

```rust
if cfg.spec().is_feature_enabled(BaseFeature::GasLimitBilling)
    && !cfg.is_fee_charge_disabled()
    && ctx.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE
{
    let gas = exec_result.gas_mut();
    gas.spend_all();
    gas.set_refund(0);
}
```

Everything downstream (EIP-7623 floor, `reimburse_caller`, operator-fee refund, `reward_beneficiary`, `ResultGas`) then observes `gas_used == gas_limit`, so the block executor, both payload builders, flashblocks pending state and receipts use the same accounting. A single settlement hook avoids duplicating fee logic across execution paths, which could otherwise produce inconsistent receipts roots between builders and validators. `eth_call` and `eth_estimateGas` run with `disable_fee_charge`, so they stay metered and estimation still converges.

**EIP-8130** ([`eip8130.rs`](../../crates/common/evm/src/eip8130.rs)). `Eip8130Executor::billable_gas` returns `FeeCheck::max_chargeable_gas(gas_limit, payer_floor)` under the feature: exactly what `prepay` pre-charged and what the builder and txpool reserve. The `simulate` estimate stays metered but stops folding `payer_floor` into the returned limit, since settlement adds it on top; a sponsored transaction sent at `gas_limit = estimate` settles `estimate + payer_floor` once.

**Builder cap** ([`execution.rs`](../../crates/builder/core/src/execution.rs)). `--builder.max_gas_per_txn` becomes a cap on declared limits: under the feature the flashblocks builder sets `ResourceLimits::max_reserved_gas_per_tx` and rejects `gas_limit + payer_floor > cap` (`TxResources::gas_limit + TxResources::payer_auth` in the builder) before execution with the permanent `MaxGasUsageExceeded`. It is also the operator's lever for bounding the worst-case fee of a padded transaction.

## Effects

Users pay `gas_limit * price` instead of `gas_used * price`; out-of-gas already paid the limit, so only the success-path discount goes away. Wallet padding becomes a visible cost. Receipts show `gasUsed == gasLimit` (`+ payer_floor` for EIP-8130), so explorers display 100% utilisation and need traces for metered gas. Header `gasUsed` becomes reserved gas, so the base fee reacts to reserved demand and blocks hold at most `block_gas_limit` of declared gas. The prover runs the same handler, so proofs follow once the feature is in the rollup config.

| RPC | Behavior |
|---|---|
| `eth_estimateGas`, `eth_call` | Unchanged (`disable_fee_charge`); the estimate is exactly what a standard transaction is billed if used as its limit. |
| `eth_simulateV1` | With fee charging enabled, a call without `gas` is reported as consuming the block's remaining gas, leaving no gas for later calls and draining the `--rpc.gascap` budget. Simulation fee-charging semantics require an explicit decision before activation. |
| `debug_trace*`, `trace_*` | Top-level `gasUsed` is the limit; per-opcode costs stay metered. Whether to expose a separate metered total is an activation consideration. |
| Receipts, blocks, `eth_feeHistory` | Report reserved gas. |

## Design invariants

All execution paths must enforce the same accounting after activation while preserving pre-activation behavior:

- The payer's fee debit equals `B * effectiveGasPrice + l1Fee + operatorFee(B)`; fee-vault and sequencer credits account for the full fee. Value transfers are separate: only committed transfers debit the sender, which may differ from the fee payer for sponsored EIP-8130 transactions.
- Receipt `gasUsed` equals `B` for user transactions. Header `gasUsed` equals the sum of reserved user-transaction gas and metered deposit gas, and agrees with the last receipt's cumulative gas.
- Builders refuse transactions whose reservation exceeds the remaining block gas or the configured per-transaction cap, even when metered execution would fit. Validators reject payloads with inconsistent gas accounting.
- EIP-3529 and EIP-7702 refunds and the EIP-7623 floor do not reduce the billed amount. Reverts and successful execution at the same reservation have the same billable gas.
- Estimation remains metered. EIP-8130 estimates exclude `payer_floor`, which settlement adds exactly once.
- Deposits retain metered gas and pay no L2 execution fee.
- Opcode metering, out-of-gas behavior, and execution results remain unchanged. Proving and block validation must use the same activated rules as payload construction.

## Design requirements

- **Upgrade gate:** activate gas-limit billing with Everest.
- **Refunds:** remove the fee effect of EIP-3529 storage-clearing, EIP-7702 authorization, and stateful-precompile refunds. Keep opcode and precompile prices unchanged; any repricing is separate work. This keeps the fee computable before execution.
- **Gas estimation:** under gas-limit billing, `eth_estimateGas` returns the sender's estimated `gas_limit`, excluding `payer_floor`, so the result can be used directly as the transaction's gas limit. Settlement adds `payer_floor` exactly once. Clients calculating the total fee must include payer authentication separately, along with the L1 data fee and operator fee; the gas estimate is not a total fee quote.

## Proposed design choices

The following policies are proposed for this design and remain subject to finalization:

- **Base-fee target and elasticity:** keep both unchanged. Reserved gas can increase reported fullness and upward base-fee pressure while clients adjust their limits; changing the fee-market parameters is a separate decision.
- **EIP-8130 payer authentication:** bill `gas_limit + payer_floor`, matching the builder's reservation.
- **Static EIP-8130 coinbase tips:** keep pool ordering by tip divided by `gas_limit`, without changing the pool. This can rank sponsored transactions higher than their revenue per unit of reserved space because the reservation also includes `payer_floor`.
- **Client compatibility:** ERC-4337 bundlers adjust `preVerificationGas` to recover sponsorship costs under gas-limit billing, rather than requiring a new Entrypoint deployment. Wallets, tooling, explorers, and searchers must account for paid estimate padding and reserved-gas reporting.

## Open decisions

The following choices must be resolved before network activation:

- **RPC**: define `eth_simulateV1` fee charging when `validation=false`, metered gas reporting in traces, and whether `Eip8130Executor::execute` must honor `disable_fee_charge` consistently with standard transactions.

Key risks are higher fees for padded limits, greater dependence on accurate estimation, searchers paying for inclusion-time uncertainty, a changed meaning of `gasUsed`, fewer transactions per block when reservations exceed consumption, and divergence from the OP Stack fee model. `--builder.max_gas_per_txn` bounds the reservation, but the fee also depends on the transaction's effective gas price.

## Activation

Gas-limit billing is a consensus change intended to activate with Everest and must activate consistently in builders, validators, and provers. Before network activation, finalize the proposed policies and resolve the remaining open decisions above, ensure the design invariants hold across the activation boundary, and define client-facing fee and gas-reporting semantics. Follow [UPGRADES.md](UPGRADES.md) for network activation, with Base Sepolia preceding Mainnet.

Amsterdam's separate gas pricing is outside the scope of Everest activation: Base will adopt Amsterdam after Everest, so compatibility with those gas dimensions can be addressed separately.

References: [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559), [EIP-3529](https://eips.ethereum.org/EIPS/eip-3529), [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623), [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825), [EIP-8130](https://eips.ethereum.org/EIPS/eip-8130); [`crates/common/evm/README.md`](../../crates/common/evm/README.md).
