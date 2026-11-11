# Gas-Limit Billing

Gas-limit billing charges a Base transaction for the gas it reserves instead of the gas it consumes. It aligns payment with the priority a transaction receives from the builder, so high-tip transactions that terminate early cannot obtain cheap inclusion. This document defines the billing rule, its implementation boundaries, alternatives, and activation considerations.

## Problem

The builder evaluates and executes candidates in priority-fee order within a fixed block time budget, including a separate validity-predicate evaluation budget. Ranking is by `tip` per gas, known at admission; payment is `tip * gas_used`, known after execution. A transaction can reserve enough gas for a costly execution path but terminate early when an opportunity is unavailable. A high tip then buys priority while the small amount of consumed gas keeps payment low. A flood of these transactions consumes evaluation and execution time, deferring other transactions.

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

1. **Billable gas** `B = gas_limit` for standard transactions and `B = gas_limit + payer_auth_cost` for EIP-8130 transactions: the quantity billed and reserved by the builder. This reservation is distinct from the pool's ranking denominator; static EIP-8130 coinbase-tip ordering continues to use `gas_limit`, not `B`. `payer_auth_cost` is the payer-authentication gas (`IntrinsicGas::payer_auth_cost_of`: the payer's authentication, plus any non-k1 `payer_auth` bytes at the EIP-7623 floor rate), metered on top of the sender's `gas_limit`; it is zero for self-paid transactions.
2. **Settlement.** The payer pays `B * effective_gas_price` plus the L1 data fee plus the operator fee on `B`. Unused gas is not reimbursed; EIP-3529 storage-clearing refunds, EIP-7702 authorization refunds, and stateful-precompile refunds do not reduce the fee. The EIP-7623 floor does not change the fee. Refund amounts depend on execution; applying them would make the final fee unknown when the builder orders transactions, defeating the design goal. The sequencer receives `B * priority_fee_per_gas`, the `BaseFeeVault` `B * basefee`, the `OperatorFeeVault` the operator fee on `B`. The L1 data fee is unchanged.
3. **Receipts and header.** Receipt `gasUsed = B`; `cumulativeGasUsed` and header `gasUsed` add `B` per user transaction, so `gasUsed * effectiveGasPrice` is still the execution fee and receipts still sum to the header.
4. **Deposits** are unchanged: no L2 execution fee, metered `gasUsed`.
5. **Unchanged:** ordering (`tip` per gas), validity rules, intrinsic gas, the EIP-7825 cap (16,777,216 gas, which already bounds `B`: `gas_limit` for standard transactions and `gas_limit + payer_auth_cost` for EIP-8130, so `B` never exceeds the cap), the opcode gas schedule and out-of-gas semantics, L1 data fee, DA footprint, batching.

Execution is still metered internally; only what is settled and reported changes. No partial rate: any fraction `p < 1` leaves the discount, and the attack, in place at scale `1/p`.

| Property | Metered | Gas-limit billing |
|---|---|---|
| Ranking key | `tip` per gas | `tip` per gas (unchanged) |
| Sequencer revenue | `tip * gas_used`, known after execution | `tip * gas_limit`, known at admission |
| Cost of outranking a transaction | `tip` on the gas you end up using | `tip` on every unit you declared |
| Block fullness (header `gasUsed`) | metered sum; declared gas in a block can exceed `block_gas_limit` | reserved sum; user transactions' declared gas is at most `block_gas_limit` (deposits keep metered gas) |

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

`BaseFeature::GasLimitBilling` is a semantic feature in the hardfork feature registry ([`feature.rs`](../../crates/common/genesis/src/feature.rs)), registered under `BaseUpgrade::Everest`. Call sites check `spec.is_feature_enabled(BaseFeature::GasLimitBilling)`. Behavior is unchanged unless the feature is active. Everest executes on the revm-based [`crates/common/evm`](../../crates/common/evm/README.md), where the hook lives. The EVM2 port ([`crates/common/evm2`](../../crates/common/evm2/README.md)) lands after Everest and must reproduce the same settlement and reporting semantics when it becomes the executor.

**Standard transactions** ([`handler.rs`](../../crates/common/evm/src/handler.rs)). One hook in `BaseHandler::post_execution`, between revm's refund step and the result gas:

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

Everything downstream (`ResultGas`, EIP-7623 floor, `reimburse_caller`, operator-fee refund, `reward_beneficiary`) then observes `gas_used == gas_limit`, so the block executor, the payload builder and receipts use the same accounting. A single settlement hook avoids duplicating fee logic across execution paths, which could otherwise produce inconsistent receipts roots between builders and validators. `eth_call`, `eth_estimateGas` and `eth_simulateV1` with `validation=false` run with `disable_fee_charge`, so they stay metered and estimation still converges.

**EIP-8130** ([`eip8130.rs`](../../crates/common/evm/src/eip8130.rs)). `Eip8130Executor::billable_gas` returns `FeeCheck::max_chargeable_gas(gas_limit, payer_auth_cost)` under the feature: exactly what `prepay` pre-charged and what the builder and txpool reserve. The `simulate` estimate stays metered but stops folding `payer_auth_cost` into the returned limit, since settlement adds it on top; a sponsored transaction sent at `gas_limit = estimate` settles `estimate + payer_auth_cost` once. `Eip8130Executor::execute` honors `disable_fee_charge` exactly like the standard handler: it still verifies signatures, validates and consumes the sender's nonce and sanity-checks the fee caps, but skips the payer's balance check, the pre-charge and every settlement transfer, and reports the metered gas net of the EIP-3529-capped refund (`Eip8130Executor::metered_gas_used`). RPC requests for EIP-8130 (`eth_call`, `eth_estimateGas`) route through `Eip8130Executor::simulate`, which never settles fees; `eth_simulateV1` rejects EIP-8130 requests. Honoring the flag in `execute` makes a fee-charge-disabled execution of a *signed* EIP-8130 transaction behave the same way, so the two executor entry points cannot diverge.

**Builder reservation** ([`builder.rs`](../../crates/execution/payload/src/builder.rs)). The payload builder already reserves `gas_limit` (`gas_limit + payer_auth` for EIP-8130) against the block gas budget before execution, so under the feature the reserved gas is also the billed gas and a transaction that does not fit on its declared limit is not executed. There is no separate per-transaction gas cap in the builder; the EIP-7825 cap bounds `B`.

## Effects

Users pay `gas_limit * price` instead of `gas_used * price`. A transaction that halts exceptionally (out of gas, invalid opcode) already paid its whole limit; successful and reverted transactions both lose the unused-gas refund. Wallet padding becomes a visible cost. Receipts show `gasUsed == gasLimit` (`+ payer_auth_cost` for EIP-8130), so explorers display 100% utilisation and receipts no longer tell a sender how much of its limit was needed. Header `gasUsed` becomes reserved gas, so the base fee reacts to reserved demand and blocks hold at most `block_gas_limit` of declared user-transaction gas. The prover runs the same handler, so proofs follow once the feature is in the rollup config.

| RPC | Behavior |
|---|---|
| `eth_estimateGas`, `eth_call` | Unchanged (`disable_fee_charge`); the estimate is exactly what a standard transaction is billed if used as its limit. |
| `eth_simulateV1`, `validation=false` | Metered (`disable_fee_charge`, alongside the nonce and base-fee checks the mode already disables): a call without `gas` reports what it consumed, so later calls keep the block's remaining gas and the request-wide `--rpc.gascap` budget is not drained. `disable_fee_charge` also skips the L1 data fee and operator fee deductions, as `eth_call` does, so an unvalidated call no longer needs balance for them. The flag is set at every fork, so this also applies before Everest. This deliberately deviates from upstream reth's `simulate_v1`, which sets only `disable_nonce_check` and `disable_base_fee`; keep it when syncing the fork. |
| `eth_simulateV1`, `validation=true` | Receipt-accurate: each call reports its gas limit, as it would when included. A call without `gas` therefore defaults to, and is accounted at, the block's remaining gas. `eth_simulateV1` rejects EIP-8130 requests in both modes. |
| `debug_trace*`, `trace_*` | Replays of included transactions run with fee charging on, so the transaction-level `gasUsed` is the billed gas `B` (`gas_limit + payer_auth_cost` for a sponsored EIP-8130 transaction) while call frames and per-opcode costs stay metered. `debug_traceCall` and `trace_call` run with `disable_fee_charge`, like `eth_call`, so they report metered gas. Summing per-opcode costs does not reproduce metered gas (intrinsic gas, refunds, and the EIP-7623 floor are not opcode costs); see recovering metered gas below. |
| Receipts, blocks, `eth_feeHistory` | Report reserved gas. |

**Recovering metered gas.** Fee-charge-disabled execution stays metered, so `eth_estimateGas` against the parent block's state returns an estimated sufficient limit for an included transaction, within the estimator's search tolerance. It runs in the parent block's environment and does not see earlier transactions in the same block, so it can differ from what the transaction needed at inclusion. That is the number a sender uses to size its next limit after an over-estimate, but it is not the metered `gasUsed`: it is gross of refunds and includes the 63/64 headroom nested calls need. `eth_call` returns only output. Separately, the builder's `BUILDER_ACCEPTED` event ([`builder.rs`](../../crates/execution/payload/src/builder.rs)) carries the metered gas of every included validity-predicate transaction alongside the billed gas, so before/after analysis does not require re-simulation: `data.gas_used` is the billed gas (`B`), `data.metered_gas_used` is the gas the transaction would have been billed without the feature (spent less the capped EIP-3529 refund, raised to the EIP-7623 floor; for EIP-8130, `Eip8130Executor::metered_gas_used`), and `data.payer_auth_gas_reserved`, present only for EIP-8130, is `payer_auth_cost`, so `gas_used == tx_gas_limit + payer_auth_gas_reserved`. Before activation `metered_gas_used == gas_used`. The handler records the metered value immediately before the hook discards it and hands it to the builder through [`MeteredGas`](../../crates/common/evm/src/metered_gas.rs), a per-transaction thread-local slot like `Eip8130CommittedPhases`; nothing on the consensus path reads it. Those events are internal telemetry: no receipt, header, state root, or RPC response changes. See [`docs/transaction-events/README.md`](../transaction-events/README.md) for the event contract.

## Design invariants

All execution paths must enforce the same accounting after activation while preserving pre-activation behavior:

- The payer's fee debit equals `B * effectiveGasPrice + l1Fee + operatorFee(B)`; fee-vault and sequencer credits account for the full fee. Value transfers are separate: only committed transfers debit the sender, which may differ from the fee payer for sponsored EIP-8130 transactions.
- Receipt `gasUsed` equals `B` for every billed transaction: the gas limit, plus `payer_auth_cost` for EIP-8130. Header `gasUsed` equals the sum of the receipts' `gasUsed` and agrees with the last receipt's `cumulativeGasUsed`.
- Builders refuse transactions whose reservation exceeds the remaining block gas, even when metered execution would fit. Validators reject payloads with inconsistent gas accounting.
- EIP-3529 and EIP-7702 refunds and the EIP-7623 floor do not reduce the billed amount. Reverts and successful execution at the same reservation have the same billable gas.
- Estimation remains metered. EIP-8130 estimates exclude `payer_auth_cost`, which settlement adds exactly once.
- Deposits retain metered gas and pay no L2 execution fee.
- Opcode metering, out-of-gas behavior, and execution results remain unchanged. Proving and block validation must use the same activated rules as payload construction.

## Design requirements

- **Upgrade gate:** activate gas-limit billing with Everest.
- **Refunds:** remove the fee effect of EIP-3529 storage-clearing, EIP-7702 authorization, and stateful-precompile refunds. Keep opcode and precompile prices unchanged; any repricing is separate work. This keeps the fee computable before execution.
- **Gas estimation:** under gas-limit billing, `eth_estimateGas` returns the sender's estimated `gas_limit`, excluding `payer_auth_cost`, so the result can be used directly as the transaction's gas limit. Settlement adds `payer_auth_cost` exactly once. Clients calculating the total fee must include payer authentication separately, along with the L1 data fee and operator fee; the gas estimate is not a total fee quote.

## Design decisions

The following policies were settled in design review:

- **Base-fee target and elasticity:** keep both unchanged. Reserved gas can increase reported fullness and upward base-fee pressure while clients adjust their limits; changing the fee-market parameters is a separate decision.
- **EIP-8130 payer authentication:** bill `gas_limit + payer_auth_cost`, matching the builder's reservation.
- **Static EIP-8130 coinbase tips:** keep pool ordering by tip divided by `gas_limit`, without changing the pool. This can rank sponsored transactions higher than their revenue per unit of reserved space because the reservation also includes `payer_auth_cost`.
- **Client compatibility:** ERC-4337 bundlers adjust `preVerificationGas` to recover sponsorship costs under gas-limit billing, rather than requiring a new Entrypoint deployment. Wallets, tooling, explorers, and searchers must account for paid estimate padding and reserved-gas reporting.
- **`eth_simulateV1`:** `validation=false` disables fee charging, like `eth_call`, and reports metered gas; `validation=true` keeps fee charging on and reports the receipt-accurate `gasUsed = gas_limit`. Callers wanting the fee a transaction will pay simulate with validation; callers sizing a limit simulate without.
- **Traces:** `debug_trace*` and `trace_*` replays of included transactions run with fee charging on, so the transaction-level `gasUsed` is the billed gas `B` and call frames stay metered. No separate metered total is exposed; a sender sizes its next limit with `eth_estimateGas` against the historical block, and analysis uses the builder's `metered_gas_used`.
- **EIP-8130 and `disable_fee_charge`:** `Eip8130Executor::execute` honors `disable_fee_charge` exactly like standard transactions: signature verification, nonce validation and the fee-cap sanity check still run and the nonce is still consumed; the payer balance check, the pre-charge and every settlement transfer are skipped; the result is the metered gas net of the capped refund. RPC simulation of EIP-8130 goes through `Eip8130Executor::simulate`, which never settles fees, so the two entry points agree whenever fee charging is disabled.

No design decisions remain open.

## Risks

Key risks are higher fees for padded limits, greater dependence on accurate estimation, searchers paying for inclusion-time uncertainty, a changed meaning of `gasUsed`, fewer transactions per block when reservations exceed consumption, and divergence from the OP Stack fee model. The EIP-7825 cap bounds the reservation, but the fee also depends on the transaction's effective gas price.

## Activation

Gas-limit billing is a consensus change that activates with Everest and must activate consistently in builders, validators, and provers. Before network activation, ensure the design invariants hold across the activation boundary and communicate the client-facing fee and gas-reporting semantics above. Follow [UPGRADES.md](UPGRADES.md) for network activation, with Base Sepolia preceding Mainnet.

The local devnet reproduces the before/after comparison: `just load-test gas-limit-billing` drives a high-tip reverting-arb cohort against regular validity traffic, `etc/scripts/devnet/gas-limit-billing-demo.sh` runs it on both sides of a late Everest block and compares per-cohort fees, latency, and the builder's predicate-budget deferrals, and `etc/scripts/devnet/gas-limit-billing-settlement-check.sh` checks that padded transfers settle at their declared limit (see the [load-tests README](../../crates/infra/load-tests/README.md#gas-limit-billing-demo)).

Amsterdam's separate gas pricing is outside the scope of Everest activation: Base will adopt Amsterdam after Everest, so compatibility with those gas dimensions can be addressed separately.

References: [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559), [EIP-3529](https://eips.ethereum.org/EIPS/eip-3529), [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623), [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825), [EIP-8130](https://eips.ethereum.org/EIPS/eip-8130); [`crates/common/evm/README.md`](../../crates/common/evm/README.md).
