# Gas Limit Pricing

This document covers gas limit pricing: charging a Base transaction for the gas it reserves (its declared `gas_limit`) instead of the gas it consumes (metered `gas_used`). The purpose is to stop the sequencer's fixed block-building time budget from being spent on transactions that pay almost nothing for it. The builder evaluates and executes candidates in priority-fee order within a fixed window, and validity-transaction predicate evaluation has its own hard budget (10 ms of each 200 ms flashblock). A searcher can bid a high fee per gas, pass the predicate check, and revert when the opportunity is gone; metered billing makes that revert nearly free, so a flood of such transactions consumes the budget ahead of transactions that would have been included and paid. Charging the declared limit makes a reverting arb cost what the execution it was sized for would have cost, so the top of the fee order, and the time budget it buys, costs its face value. The doc describes the problem, the rule, the design decisions, and a proof of concept (POC) gated on the Zenith experimental upgrade so the change can be exercised on a devnet before it is specified as a real network upgrade.

## Contents

- [Summary](#summary)
- [Problem: a fixed time budget spent on transactions that pay for none of it](#problem-a-fixed-time-budget-spent-on-transactions-that-pay-for-none-of-it)
- [Where the fee is decided today](#where-the-fee-is-decided-today)
- [Proposed rule](#proposed-rule)
- [What the rule fixes](#what-the-rule-fixes)
- [Alternatives considered](#alternatives-considered)
- [Proof of concept](#proof-of-concept)
  - [Activation](#activation)
  - [Change 1: standard transactions](#change-1-standard-transactions)
  - [Change 2: EIP-8130 transactions](#change-2-eip-8130-transactions)
  - [Change 3: builder per-transaction gas cap](#change-3-builder-per-transaction-gas-cap)
  - [What does not change](#what-does-not-change)
- [Effects](#effects)
- [Validation plan](#validation-plan)
- [Risks and open questions](#risks-and-open-questions)
- [Rollout](#rollout)
- [References](#references)

## Summary

Decisions:

- **The target is the builder's time budget.** Candidates are evaluated and executed in priority-fee order inside a 200 ms flashblock, with a 10 ms hard budget for validity-predicate evaluation. A high-tip transaction that passes its predicate and then reverts is included for `tip * gas_used` on a small `gas_used`, so a flood of them takes the budget from transactions that would have paid. Charging the limit makes the reverting arb cost what its execution path would have cost.
- **Billable gas is the declared gas limit.** Every non-system fee component that is priced per gas (EIP-1559 base fee, priority fee, operator fee) is charged on `gas_limit`, with no post-execution reimbursement. The L1 data fee is unchanged.
- **Ordering is unchanged.** The pool and builder keep ranking by priority fee per gas. The rule does not change how transactions are ranked; it changes what a rank costs, so that the ranking key (`tip` per gas) times the reserved gas is exactly what the sender pays.
- **`gasUsed` reports billable gas.** Receipt `gasUsed`, `cumulativeGasUsed`, and header `gasUsed` count declared gas limits for user transactions, so `fee = gasUsed * effectiveGasPrice + l1Fee + operatorFee` still holds and the block fills by the capacity the builder reserved and the senders paid for.
- **Deposits are untouched.** Deposits are not ordered by tip and pay no L2 execution fee, so they keep metered gas and their Regolith receipt shape.
- **No partial rate.** The full limit is charged. Any fraction leaves the discount, and the attack, in place at a smaller scale.
- **Base fee target and elasticity are unchanged in the POC.** Header `gasUsed` becomes reserved gas, so the base fee reacts to reserved demand; the mainnet data scan quantifies the shift before any parameter change is decided.
- **Activation gate is `BaseUpgrade::Zenith`.** Genesis-only, devnet and system tests only. Not scheduled on Base Sepolia or Base Mainnet.

The POC touches two consensus sites in `base-common-evm` (a hook in `BaseHandler::refund` and a branch in `Eip8130Executor::billable_gas`) and moves one builder check (`--builder.max_gas_per_txn`) in front of execution. RPC simulation keeps metered gas because it runs with fee charging disabled.

## Problem: a fixed time budget spent on transactions that pay for none of it

In one paragraph: the builder has a fixed 10 ms budget per flashblock for evaluating validity-transaction predicates. Every validity transaction in the pool needs some of that time before it can be included, and the budget is handed out in priority-fee order. The transactions that get prioritised are the ones bidding the highest fee per gas, and today a searcher can bid very high because the transaction reverts when its opportunity is gone, uses little gas, and therefore pays little. So the 10 ms goes to transactions that revert and pay almost nothing, the budget runs out, and the transactions behind them, the ones that would have executed, used a lot of gas, and paid for it, are deferred and never included in that flashblock. Pricing by `gas_limit` fixes this: a reverting transaction pays the same as the execution it was sized for, so bidding high on a reverting arb that usually reverts stops being cheap, and the budget flows to transactions whose bids are real.

### How the builder spends its time

A flashblock is built in a 200 ms window. The builder pulls candidates from the pool in priority-fee order and, for each one, does work before it knows whether the transaction is worth anything:

1. **Predicate evaluation** for validity transactions (EIP-8130 transactions that carry validity conditions such as "pool price above X"). The builder reads the state the predicate names and decides whether the transaction is live. This has a hard per-flashblock budget, `predicate_eval_hard_cutoff`, 10 ms by default ([`crates/builder/core/src/config.rs`](../../crates/builder/core/src/config.rs)). Once cumulative predicate-evaluation time reaches it, every remaining validity-gated candidate in that flashblock is deferred without being evaluated ([`flashblocks/context.rs`](../../crates/builder/core/src/flashblocks/context.rs), `predicate_eval_budget_exhausted`), and so is every parked transaction that a committed state change would otherwise have woken. The budget exists because, without it, predicate evaluation alone was pushing p99 build time to 190 ms of the 200 ms window.
2. **Execution** of candidates whose predicates matched (or that have none). Only now does the builder learn `gas_used` and whether the call reverted.

Both steps are consumed in priority-fee order. Whoever bids the highest fee per gas is evaluated first and executed first, and whatever time they take is unavailable to everyone behind them. The loop ends when the flashblock timer fires, whether or not it has reached the rest of the pool.

### How ordering and payment are decided

A sender signs three gas-related fields: `gas_limit`, `max_fee_per_gas`, and `max_priority_fee_per_gas` (or a legacy `gas_price`). None of them says how much gas the transaction will use. The pool ranks before anything executes:

```text
effective_tip_per_gas = min(max_fee_per_gas - base_fee, max_priority_fee_per_gas)   // EIP-1559
                      = gas_price - base_fee                                          // legacy
```

[`UnifiedTipOrdering::priority`](../../crates/execution/txpool/src/ordering.rs) uses this rate as the ranking key for standard transactions (`gas_limit` is not in the key) and `coinbase_tip / gas_limit` for statically decodable EIP-8130 coinbase tips. After execution the sender pays `effective_tip_per_gas * gas_used` to the sequencer and `base_fee * gas_used` to the `BaseFeeVault`, and the pre-charged `gas_limit - gas_used` is refunded. A reverted transaction is included and pays the same way, on the gas it consumed before reverting.

```text
rank    = effective_tip_per_gas              known when the transaction arrives
payment = effective_tip_per_gas * gas_used   known after it executes
```

### The useless transaction

An atomic arbitrage searcher sends a transaction that checks whether an opportunity exists and executes it if so. The execution path needs a lot of gas, so the declared `gas_limit` is sized for it (about 40x the median gas actually used, per the sequencer team's mainnet measurements). The check path is short: when the opportunity is gone by the time the transaction runs, it reverts after a few tens of thousands of gas.

Under metered billing the two outcomes cost very differently:

| Outcome | `gas_used` | Pays the sequencer |
|---|---|---|
| Opportunity exists, executes | ~500,000 | `tip * 500,000` |
| Opportunity gone, reverts | ~25,000 | `tip * 25,000` |

So the searcher can bid a very high `tip`, 20x the market, and still pay less for a losing reverting arb than a regular transaction pays for a successful one. The high tip puts the reverting arb at the front of the order; the revert keeps it cheap; the large `gas_limit` keeps the option to execute if the opportunity is there. Send many of them, from many accounts, every flashblock.

The effect on the builder:

- The predicate budget is spent on the reverting arbs. They sort first, so their predicates are evaluated first; by the time 10 ms is gone, candidates further down the order are deferred unevaluated, including ones whose predicates would have matched and whose execution would have paid full fees.
- The execution window is spent on the reverting arbs that pass. Each one is executed, reverts, and is included for `tip * 25,000`.
- Competing searchers and ordinary senders are either not considered or, to be considered, must bid a tip that they pay on all of the gas they actually use.

Block gas is not what runs out. Admission checks `cumulative_gas_used + gas_limit` against the flashblock budget, but after execution only metered gas is added (`info.cumulative_gas_used += gas_used`), so a 2,000,000-limit reverting arb that uses 25,000 leaves 1,975,000 for the next transaction. The scarce resource is time, and the fee market does not price it, because the fee is settled on a quantity (`gas_used`) that a reverting transaction keeps small on purpose.

### What charging the limit does

Charge `tip * gas_limit` instead, and the two outcomes cost the same:

| Outcome | Pays the sequencer under gas limit pricing |
|---|---|
| Opportunity exists, executes | `tip * gas_limit` |
| Opportunity gone, reverts | `tip * gas_limit` |

A 20x-tip reverting arb sized for 500,000 gas now pays `20 * tip * 500,000` every time it is included, win or lose: 20x what the regular transaction pays, which is what a 20x bid should cost. The searcher's options are to bid a tip they are willing to pay on the whole limit (so they no longer sit ahead of everyone for free), to tighten predicates so the transaction is not included unless the opportunity is really there (so it stops being executed and reverting), or to shrink the limit (and lose the ability to execute from that slot). Each of these takes reverting arbs out of the front of the order, which is where the budget was going.

Worked example at a base fee of 0.01 gwei and a market tip of 1 gwei:

| | Arbitrage that executes | Reverting arb |
|---|---|---|
| `gas_limit` | 600,000 | 2,000,000 |
| `gas_used` | 500,000 | 25,000 |
| `tip` (priority fee per gas) | 1 gwei | 20 gwei |
| Rank | second | first |
| Pays the sequencer today (`tip * gas_used`) | 0.0005 ETH | 0.0005 ETH |
| Pays the sequencer under gas limit pricing (`tip * gas_limit`) | 0.0006 ETH | 0.04 ETH |

Today the reverting arb buys a 20x higher rank for the same money. Under gas limit pricing the reverting arb pays 80x what it paid before and its rank costs 67x what the regular transaction pays.

### What the rule does and does not fix

It fixes the profitable version of the problem: a reverting arb can no longer be cheap, so sending many of them at a high tip is no longer a winning strategy, and the predicate and execution budgets go to transactions whose bids are real. It also makes ordering transparent: the price of a position is `tip * gas_limit`, computable from public inputs before submission, and the sequencer knows a transaction's revenue before executing it.

It does not price predicate evaluation itself. A validity transaction whose predicate does not match is not included and pays nothing under either rule, so a sender who keeps submitting high-tip transactions with predicates that rarely match still consumes evaluation time for free; what the rule removes is the incentive to make those predicates loose. And it does not change the build loop: a griefer who sends tight-limit (25,000 gas) high-tip reverting transactions pays the same under either rule and still costs one candidate's worth of time each. Both residuals are builder time-budget problems (pricing or capping predicate evaluation per sender, per-candidate execution budgets) rather than fee-rule problems, and they are out of scope here.

## Where the fee is decided today

All consensus fee logic lives in `base-common-evm`, which is `no_std` and shared by the node, the builder, the prover, and the acceptance tests.

### Standard transactions (`BaseHandler`)

[`crates/common/evm/src/handler.rs`](../../crates/common/evm/src/handler.rs):

| Phase | Today | Depends on `gas_used`? |
|---|---|---|
| `validate_against_state_and_deduct_caller` | Checks the caller can afford `gas_limit * max_fee_per_gas`, then deducts `gas_limit * effective_gas_price` (revm `calculate_caller_fee`), the L1 data fee, and the operator fee computed on `gas_limit` (`L1BlockInfo::tx_cost`). | No. The pre-charge is already a gas-limit charge. |
| `refund` | revm `post_execution::refund`: adds the EIP-7702 refund and caps the refund at `gas_used / 5` (EIP-3529). | Yes. |
| `eip7623_check_gas_floor` | Raises spent gas to the calldata floor if execution used less. | Yes. |
| `reimburse_caller` | Returns `(remaining + refunded) * effective_gas_price` plus the operator fee refund (`L1BlockInfo::operator_fee_refund`: operator fee on `gas_limit` minus operator fee on `gas_used`). | Yes. |
| `reward_beneficiary` | Pays the sequencer `priority_fee * gas_used`, the `BaseFeeVault` `basefee * gas_used`, the `L1FeeVault` the L1 cost, and the `OperatorFeeVault` the operator fee on `gas_used`. | Yes. |
| `execution_result` | Builds `ResultGas`; `tx_gas_used = max(total_spent - refunded, floor)` becomes receipt `gasUsed`. | Yes. |

The sender is charged the limit up front and then handed back the unused part. The rule below keeps the pre-charge and removes the hand-back.

Receipt `gasUsed` is already a billing quantity rather than raw execution gas: it is post-refund and post-floor. Redefining it as `gas_limit` extends that semantics rather than inventing a new one.

### EIP-8130 transactions (`Eip8130Executor`)

[`crates/common/evm/src/eip8130.rs`](../../crates/common/evm/src/eip8130.rs) has its own settlement. `settle_fees` computes `billable_gas = max(sender_intrinsic + call_gas_spent - capped_refund, sender_floor) + payer_auth`, charges the payer that amount at the effective price, routes base fee, tip, L1 cost, and operator fee to their recipients, and refunds the surplus of the worst-case pre-charge. `simulate` (the estimate path) uses the gross metered amount and is read-only.

### Block accounting

[`BaseBlockExecutor::commit_transaction`](../../crates/common/evm/src/executor/block_executor.rs) adds `result.tx_gas_used()` to the running `gas_used`, which becomes receipt `cumulativeGasUsed` and header `gasUsed`. The next base fee is computed from the parent header's `gasUsed` ([`compute_jovian_base_fee`](../../crates/execution/chainspec/src/basefee.rs)). The builder reserves by limit at admission and accounts by metered gas afterwards ([`ExecutionInfo`](../../crates/builder/core/src/execution.rs)), so admission and accounting disagree by `gas_limit - gas_used` per transaction, and a block can admit far more declared gas than its limit as long as the metered sum fits.

### RPC

reth's `eth_call` and `eth_estimateGas` set `cfg.disable_fee_charge = true` before executing. `eth_estimateGas` reads `res.result.tx_gas_used()` from an unconstrained execution to seed its binary search; if that value were `gas_limit`, the search would collapse and return the cap. The POC therefore couples gas limit pricing to fee charging being enabled. `eth_simulateV1` does not set the flag in the pinned reth (it only disables the nonce and base-fee checks when `validation` is false), so it observes the rule; see [Effects](#effects).

## Proposed rule

For every user transaction in a block where gas limit pricing is active:

1. **Billable gas** `B = gas_limit` for standard transactions and `B = gas_limit + payer_auth` for EIP-8130 transactions: the quantity the pool ranks by and the builder reserves.
2. **Settlement.** The caller pays `B * effective_gas_price`. No part of the EIP-1559 pre-charge is reimbursed. EIP-3529 refunds and the EIP-7623 floor have no effect on the fee (both are bounded by `gas_limit`). The sequencer receives `B * priority_fee_per_gas`; the `BaseFeeVault` receives `B * basefee`; the `OperatorFeeVault` receives the operator fee schedule (`L1FeeParams::operator_fee_charge_inner`) applied to `B`, with no post-execution refund. The L1 data fee is unchanged.
3. **Receipts and header.** Receipt `gasUsed = B`; `cumulativeGasUsed` and header `gasUsed` add `B` for each user transaction.
4. **Deposits** are unchanged: no L2 execution fee, metered `gasUsed`.
5. **Unchanged:** transaction ordering, transaction validity rules, intrinsic gas, nonce and balance checks, the EIP-7825 per-transaction gas cap, the opcode gas schedule, `GAS`, call stipends and the 63/64 rule, out-of-gas semantics, L1 data fee, the Jovian DA footprint, and batch submission.

Execution is still metered internally, so a transaction behaves identically; only what is settled and reported changes.

## What the rule fixes

With the rule, the pool's ranking key and the sender's payment are the same number multiplied by the same gas:

| Property | Today | Under gas limit pricing |
|---|---|---|
| Ranking key | `tip` per gas | `tip` per gas (unchanged) |
| Gas the pool and builder reason about | `gas_limit` | `gas_limit` (unchanged) |
| Sequencer revenue from the transaction | `tip * gas_used`, known after execution | `tip * gas_limit`, known at admission |
| Cost of outranking a transaction | `tip` on whatever gas you end up using | `tip` on every unit of gas you declared |
| Cost of reserving `G` gas of block capacity | the gas you use, not `G` | `G * effective_gas_price` |

Consequences:

- **The time budget goes to bids that are real.** A transaction at the front of the priority-fee order now pays its bid on its whole declared limit whether it executes or reverts, so holding the front of the order with reverting arbs costs the full price of the executions they were sized for. Fewer reverting arbs are sent at high tips, and the predicate and execution budgets reach transactions further down the order.
- **Fair ordering.** For standard transactions, ranking by `tip` is exactly ranking by protocol tip revenue per unit of reserved block space, because that revenue is `tip * gas_limit`. A transaction cannot outrank another while paying less per unit of the space it reserved; the only way to rank higher is to pay more per gas on all the gas you declare. Static EIP-8130 coinbase tips are ranked by `coinbase_tip / gas_limit` while the reservation is `gas_limit + payer_auth` and settlement also credits the priority fee, so for them the ranking key is an approximation of revenue per reserved gas rather than exactly it; see [Risks](#risks-and-open-questions).
- **Transparent ordering.** A sender can compute what any position in the block costs from public information (`tip`, `gas_limit`, the base fee) before submitting. Nothing about the price depends on how the transaction runs.
- **Spam and reverting arbs are priced.** A padded reverting arb pays `gas_limit / gas_used` times more than today (80x in the example above, where its rank also ends up costing 67x what the regular transaction pays). Reserving the block's admission budget costs the full price of that budget. Reverted and early-exit transactions pay the same as successful ones for the same declared limit.
- **Searchers adapt by tightening.** The rational response is a limit sized for the real path and predicates checked as cheaply and early as possible, or off-chain. Loose bounds are still allowed; they just cost what they reserve. Searchers already optimize gas; the limit becomes one more thing to optimize.
- **Builder effort tracks revenue.** The builder knows a transaction's tip revenue (`tip * B`) before executing it, so the high-`tip` transactions it executes first are the ones that pay the most per unit of the capacity they take. Early exits no longer convert admitted capacity into a refund, and the per-transaction gas cap can be decided before execution ([Change 3](#change-3-builder-per-transaction-gas-cap)). `gas_headroom` metrics become exact because admission and accounting agree. The rule prices reservation; it does not claim to order transactions by total payment or by execution effort, and it does not change the build loop: fewer reverting arbs reach it because they are no longer profitable, not because it can skip them.
- **Capacity is paid for.** Header `gasUsed` counts reserved gas, so a block full by reservation reads as full to EIP-1559 whenever execution gas is the binding term of the Jovian base-fee input (`max(gasUsed, blobGasUsed)`). The base fee then responds to the demand for block space that was actually allocated, and senders are paid back for tightening limits with more transactions per block.

## Alternatives considered

| Alternative | Rule | Why not |
|---|---|---|
| Rank by total bid, `tip * gas_limit` | Keep metered payment; order by the product. | Ranks a large transaction with a low per-gas tip above small ones, so it is not a per-gas market any more; and the padded reverting arb still pays `tip * gas_used`, so reserving capacity stays cheap. The ranking changes but the payment gap remains. |
| Rank by realised payment, `tip * gas_used` | Execute, then order by what was paid. | Requires executing every candidate before ordering it, which is the builder cost this change wants to remove, and the reverting arb still pays little for the space it reserved. Also makes ordering depend on execution, so a sender cannot know its position's price before submitting. |
| Penalise reverts | Charge the limit only when the transaction fails. | A speculative transaction that exits early with `STOP` is a successful transaction. The pattern does not need a revert. |
| Simulate before ordering | Execute candidates first, then order by what they actually pay. | Simulation is the time the builder is running out of; this spends more of it per candidate, not less. |
| Price predicate evaluation | Charge validity transactions for each predicate check, matched or not. | Complementary, not a substitute: it prices the evaluation step but leaves a passed-then-reverted reverting arb cheap. Worth pursuing separately for the residual described in [the problem section](#what-the-rule-does-and-does-not-fix). |
| Pool heuristics on `gas_limit / gas_used` | Track senders' past utilisation and deprioritise padders. | Gameable with fresh addresses; punishes regular senders whose gas is state-dependent; and adds per-sender state to the pool's hot path. Does not change what anyone pays. |
| Partial charge | Bill `gas_used + p * (gas_limit - gas_used)`. | For any `p < 1` the reverting arb still pays less per unit of rank than the regular transaction, by a factor of about `1/p`; the gap shrinks but stays. Could inform a transition if the data scan shows a large shock, at the cost of the invariants above. |
| Fee on `gas_limit`, `gasUsed` stays metered | Change only `reimburse_caller` and `reward_beneficiary`. | Breaks `fee = gasUsed * effectiveGasPrice`, which explorers and wallets compute today, so every fee display becomes wrong. Header `gasUsed` stays metered, so the block keeps admitting far more declared gas than its limit and the base fee never sees the reserved demand. |
| Header `gasUsed` from limits, receipts metered | Keep receipt `gasUsed` metered but count limits in the header. | Receipts no longer sum to the header, so the receipts root and header disagree in a way no client expects. Two notions of `gasUsed` in one block. |
| Price deposits by limit too | Apply the rule to every transaction type. | Deposits pay no L2 fee and are not ordered by tip, so there is nothing to fix; changing their receipt shape is a Regolith regression for no benefit. |

## Proof of concept

Status: implemented in `base-common-evm` and `base-builder-core` behind `BaseSpecId::charges_gas_limit()` (Zenith), with the Tier 1 and Tier 2 tests below. System tests and the mainnet data scan are not yet implemented.

### Activation

Gate on `BaseUpgrade::Zenith` ([`crates/common/genesis/src/chain/upgrade.rs`](../../crates/common/genesis/src/chain/upgrade.rs)), the "hardfork for future experimental features". It is genesis-configurable only, not recognised by the L1 upgrade-signal contract, and already gates the EIP-8130 Keystore experiment. The system-test harness activates it through `L2_BASE_ZENITH_BLOCK` ([`etc/systems/src/setup/container.rs`](../../etc/systems/src/setup/container.rs)).

Name the feature once so call sites do not spell the fork:

```rust
// crates/common/evm/src/spec.rs
impl BaseSpecId {
    /// Returns true if transactions are billed for their declared gas limit
    /// rather than their metered gas. See docs/guides/GAS_LIMIT_PRICING.md.
    pub const fn charges_gas_limit(self) -> bool {
        self.is_enabled_in(BaseUpgrade::Zenith)
    }
}
```

When the feature graduates, the predicate moves to the named upgrade and nothing else changes.

### Change 1: standard transactions

Hook `BaseHandler::refund`. revm 43's `post_execution` runs `refund -> build ResultGas -> eip7623_check_gas_floor -> reimburse_caller -> reward_beneficiary`, so zeroing remaining gas and the refund counter in `refund` makes every later step, including the `ResultGas` that becomes the receipt, observe `gas_used = gas_limit`. `Gas::spend_all` clears only the remaining regular gas, so the refund counter must be zeroed explicitly.

```rust
// crates/common/evm/src/handler.rs
fn refund(
    &self,
    evm: &mut Self::Evm,
    exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    eip7702_refund: i64,
) -> Result<(), Self::Error> {
    post_execution::refund(evm.ctx().cfg().gas_params(), exec_result.gas_mut(), eip7702_refund);

    // Gas limit pricing: a non-deposit transaction is billed and reported
    // for the whole declared budget it reserved. Deposits keep metered gas;
    // simulation (`disable_fee_charge`) keeps metered gas so
    // `eth_estimateGas` still finds the gas a call needs.
    let ctx = evm.ctx();
    let cfg = ctx.cfg();
    if cfg.spec().charges_gas_limit()
        && !cfg.is_fee_charge_disabled()
        && ctx.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE
    {
        let gas = exec_result.gas_mut();
        gas.spend_all();
        gas.set_refund(0);
    }

    Ok(())
}
```

Consequences, with no further code changes:

- `eip7623_check_gas_floor`: spent gas is `gas_limit >= floor`; no-op.
- `reimburse_caller`: `remaining + refunded = 0`, so the EIP-1559 reimbursement is zero. `operator_fee_refund` is the operator fee on `gas_limit` minus the operator fee on `gas_limit - 0`: zero.
- `reward_beneficiary`: tip, base fee, and operator fee are computed from `gas().used() == gas_limit`.
- `ResultGas::tx_gas_used = max(gas_limit - 0, floor) = gas_limit`; receipts and header follow. The builder's post-execution `cumulative_gas_used` now increases by exactly what admission reserved.
- Deposits skip the hook, so their settlement (zero gas price, no reward) and metered receipt are exactly as before.
- `Gas::spend_all` does not touch the EIP-8037 reservoir. EIP-8037 is disabled on every Base upgrade (`test_eip8037_state_gas_disabled_on_all_base_upgrades`); if Base ever maps to Amsterdam, state gas needs its own decision.

### Change 2: EIP-8130 transactions

`Eip8130Executor::billable_gas` is the single source of the settled amount. Add the spec and return the reserved budget:

```rust
// crates/common/evm/src/eip8130.rs
fn billable_gas(outcome: &Eip8130Outcome, calls: &CallsResult, spec: BaseSpecId) -> u64 {
    if spec.charges_gas_limit() {
        // The amount the pool ranks by and the builder reserves: the sender
        // budget plus payer authentication metered on top of it.
        return outcome.gas_limit.saturating_add(outcome.payer_auth);
    }
    // existing net charge
    ...
}
```

`settle_fees` already derives the fee, base-fee amount, tip, and operator fee from `billable_gas`, refunds `prepay - total_cost` to the payer, and returns `billable_gas` as the receipt gas. The pre-charge bounds `gas_limit + payer_auth` at the effective price, so the subtraction cannot underflow.

`simulate` (EIP-8130 estimation) stays on the gross metered amount, with one adjustment: today it returns `sender_budget + payer_auth` as the suggested `gas_limit`, folding payer authentication in as call-pool headroom that costs nothing under metered billing. Under gas limit pricing that limit is billed as-is and `payer_auth` is billed on top, so a transaction submitted at `gas_limit = estimate` would pay for payer authentication twice. When `charges_gas_limit()` is active the estimate is therefore the sender budget alone; executing at that limit settles exactly `estimate + payer_auth`.

### Change 3: builder per-transaction gas cap

The operator's per-transaction gas cap (`--builder.max_gas_per_txn`) is checked against `tx_gas_used()` after execution. Under the rule that value equals the declared limit, so the check can be made before executing: when `charges_gas_limit()` is active the builder sets `ResourceLimits::tx_gas_limit` from `max_gas_per_txn` and [`ExecutionInfo::is_tx_over_limits`](../../crates/builder/core/src/execution.rs) rejects a transaction whose reserved `gas_limit + payer_auth` exceeds it without running it, with the same permanent `MaxGasUsageExceeded`. Without the feature the check stays where it is.

The cap thereby becomes a cap on declared limits. Operators that set it should expect wallet-padded transactions above it to be excluded, which is the intended meaning of a per-transaction gas cap under reservation pricing, and it is also the operator's lever for bounding the worst-case fee a padded transaction can pay (see [Risks](#risks-and-open-questions)).

### What does not change

- Transaction pool ordering ([`crates/execution/txpool`](../../crates/execution/txpool)): `UnifiedTipOrdering` keeps ranking by `effective_tip_per_gas`, or by `coinbase_tip / gas_limit` for static EIP-8130 tips. The rule makes the standard-transaction ranking revenue-exact; it does not touch the ordering code.
- Transaction validity, intrinsic gas, nonce and balance checks, and the EIP-7825 cap: the pool and handler balance check already requires `gas_limit * max_fee + value + L1 fee + operator fee`, so a transaction that is admissible today can pay the new fee.
- Builder admission: it already reserves `gas_limit (+ payer_auth)` against the block gas limit.
- Opcode gas schedule, `GAS` opcode, call stipends, and the 63/64 rule: the EVM meters gas internally so execution is identical.
- L1 data fee, DA footprint (Jovian), and batch submission.
- Deposits, the L1 attributes transaction, and deposit fee semantics.
- Derivation: the consensus layer does not compute gas.

## Effects

**Users.** The execution component of the fee rises from `gas_used * price` to `gas_limit * price`. A transaction that fails out-of-gas already pays its full limit today, so the worst case is unchanged; the change removes the success-path discount. Wallet padding becomes a direct cost, so wallets should move toward tight `eth_estimateGas` results. Trimming too far turns into out-of-gas failures that still pay the full limit; this tradeoff is measured in the validation plan.

**Searchers and bots.** The cost of a position in the block is `tip * gas_limit`, computable before submission. Reverting arbs and loosely bounded transactions pay for the gas they declare whether they execute or revert, so limits should be sized for the real path and predicates made tight enough that the transaction is not included unless the opportunity is there. A high `tip` on a padded limit no longer buys cheap priority or cheap evaluation time. Expect atomic arbitrage transaction counts to fall and their revert rate to fall with them; the sequencer team's expectation is that revenue from that segment stays roughly flat because bids are discounted for revert risk and opportunities are acted on more selectively.

**Receipts and explorers.** `gasUsed == gasLimit` for every standard user transaction, and `gasUsed == gasLimit + payer_auth` for sponsored EIP-8130 transactions. `effectiveGasPrice * gasUsed + l1Fee (+ operatorFee)` still equals the amount debited. Explorers that display "gas used / gas limit" utilisation will show 100% for user transactions; they need traces or `eth_estimateGas` for metered gas. Deposit receipts are unchanged.

**Base fee.** Header `gasUsed` becomes reserved gas for user transactions. Jovian feeds `max(gasUsed, blobGasUsed)` into the EIP-1559 update ([`compute_jovian_base_fee`](../../crates/execution/chainspec/src/basefee.rs)), so the base fee rises where execution gas is the binding term and is unaffected in blocks where the DA footprint already dominates. Target and elasticity are unchanged in the POC; the expectation is a higher base fee after activation until limits tighten, and the data scan below tests that expectation rather than assuming it.

**Sequencer and vaults.** Tip revenue and `BaseFeeVault` inflows increase by each user transaction's `gas_limit - gas_used` at its own tip and base fee. The operator fee is computed on `gas_limit` and no longer refunded.

**Builder.** Revenue per transaction is known at admission. `max_gas_per_txn` is enforced before execution. Admission and accounting agree, so a block admits at most `block_gas_limit` of declared gas; today it can admit more as long as the metered sum fits. Expect fewer, better-paid transactions per block until wallets and searchers tighten limits.

**RPC.**

| Endpoint | Behavior |
|---|---|
| `eth_estimateGas` | Unchanged: runs with `disable_fee_charge`, so the hook does not fire and the search sees metered gas. The result is now also exactly what a standard transaction is billed for if it is used as the limit (sponsored EIP-8130 transactions are billed `payer_auth` on top; see [Risks](#risks-and-open-questions)). |
| `eth_call` | Unchanged for the same reason. |
| `eth_simulateV1` | Observes the rule in both `validation` modes, because the pinned reth never sets `disable_fee_charge` for it (it only disables EIP-3607, the nonce check, and the base-fee check): each simulated call's `gasUsed` is its gas limit, matching a real block. A call that omits `gas` is given the block's remaining gas and is therefore reported as consuming all of it, so later calls in the same simulated block default to a limit of 0, and the full-limit `gasUsed` is also subtracted from the request-wide `--rpc.gascap` budget (`GasLimitReached`). Base owns its `simulate_v1` ([`crates/execution/rpc/src/eth/call.rs`](../../crates/execution/rpc/src/eth/call.rs)); whether it should disable fee charging when `validation` is false is an open follow-up, not changed by the POC, and needs a multi-call `eth_simulateV1` test under Zenith either way. |
| `eth_getTransactionReceipt`, `eth_getBlockByNumber` | `gasUsed`, `cumulativeGasUsed`, header `gasUsed` report billable gas for user transactions. |
| `debug_traceTransaction`, `trace_*` | Replay with fee charging enabled; top-level `gasUsed` equals `gas_limit`. Per-opcode `gas`/`gasCost` are metered and unchanged. |
| `eth_feeHistory` `gasUsedRatio` | Reflects reservation, which is the intended signal. |

**Proving.** The prover executes the same `base-common-evm` handler, so proofs follow once the fork is active in the rollup config used by the proof program. No separate change.

## Validation plan

The POC is validated at the four tiers in [TESTING.md](TESTING.md), plus a mainnet data scan. Every test runs twice where it matters: once with Zenith active and once with the latest scheduled upgrade (`Everest`), to prove the pre-activation path is untouched.

### Success criteria

The POC works when all of the following hold on a devnet with Zenith active, and none of them changes behavior on a devnet without Zenith:

| # | Criterion | Checked by |
|---|---|---|
| S1 | For every user transaction, `sender_balance_before - sender_balance_after == value + gasLimit * effectiveGasPrice + l1Fee + operatorFee(gasLimit)`, independent of how much gas execution used. | Unit (handler), system test |
| S2 | Receipt `gasUsed == gasLimit` for every user transaction; `cumulativeGasUsed` is the running sum; header `gasUsed` equals the last receipt's `cumulativeGasUsed`. | Block executor test, system test |
| S3 | Header `gasUsed` equals the sum of declared limits of the block's user transactions plus the metered gas of its deposits, so the block fills by reserved capacity. | Block executor test, system test |
| S4 | Fee recipients receive gas-limit-based amounts: sequencer `tip * gasLimit`, `BaseFeeVault` `basefee * gasLimit`, `OperatorFeeVault` the operator fee on `gasLimit`, `L1FeeVault` unchanged. Sum of credits equals the sender debit minus `value`. | Unit (handler), system test |
| S5 | EIP-3529 refunds and the EIP-7623 floor have no effect on the fee or on `gasUsed`. | Unit (handler) |
| S6 | `eth_estimateGas` returns the metered requirement (about 21,000 for a plain transfer), not the cap; `eth_call` returns metered gas. | System test |
| S7 | Builder and validator agree: every block the builder produces is accepted by the validator, and a payload whose header `gasUsed` is the metered sum is rejected as invalid after activation. | System tests (`fuzz_sync_parity`-style and `invalid_new_payload`-style) |
| S8 | EIP-8130: settled charge is `gasLimit + payer_auth` at the effective price; `simulate` still returns the metered estimate. | Unit (eip8130), system test (`eip8130`) |
| S9 | Deposits pay nothing, report metered gas, and the L1 attributes transaction executes unchanged. | Unit (handler) |
| S10 | A proof of a Zenith block containing gas-limit-priced transactions verifies. | System test (`in_process_zk`-style) |
| S11 | Out-of-gas behavior, revert behavior, and all per-opcode gas in traces are identical with and without Zenith. | Unit (handler), trace comparison in the system test |
| S12 | Without Zenith, the full workspace test suite passes with no assertion changes. | `just test`, `just devnet tests` |
| S13 | A reverting arb pays its declared limit: a standard or EIP-8130 transaction whose call reverts after a small amount of gas is billed `tip * gasLimit` (`+ payer_auth`) and the sequencer receives it, the same as a successful execution at that limit; metered under Everest. For any two standard user transactions in a block, the one ranked higher paid at least as much tip per unit of reserved gas. | Unit (handler, eip8130), system test |
| S15 | Under a flood of high-tip reverting validity transactions, the predicate-evaluation cutoff is hit and lower-tip candidates are deferred under Everest while the reverting arbs are included for metered gas; under Zenith each included reverting arb pays its full declared limit. | System test (reverting-arb flood) |
| S14 | A transaction whose reserved gas exceeds `--builder.max_gas_per_txn` is rejected before execution under Zenith and after execution otherwise. | Unit (builder) |

### Tier 1: unit tests

In `handler.rs` (`#[cfg(test)] mod tests`), executing single transactions against an `InMemoryDB` with a funded caller and reading the caller, beneficiary, and `BaseFeeVault` balances afterwards:

- Zenith, plain transfer with `gas_limit = 100_000`: `tx_gas_used == 100_000`; caller debited `100_000 * effective_gas_price`; beneficiary credited `100_000 * tip`; `BaseFeeVault` credited `100_000 * basefee` (S1, S2, S4).
- Zenith with an SSTORE clear that earns an EIP-3529 refund: refund is zero and the settlement is the same as the plain transfer (S5).
- Zenith, out of gas: identical settlement to Everest, since out of gas already bills the limit (S11).
- Zenith, the reverting arb shape (call reverts after its check): billed and reported at the full declared limit, sequencer paid `tip * gas_limit`; metered and cheap on Everest (S13).
- Zenith, `disable_fee_charge`: `tx_gas_used == 21_000`, nothing debited (S6).
- Zenith deposit (`source_hash` set): `tx_gas_used == 21_000` as on Everest; nothing debited, nothing credited (S9).
- Everest: the plain transfer is billed 21,000; `test_consume_gas`, `test_revert_gas`, `test_consume_gas_with_refund`, and the rest of the existing module pass unchanged (S12).
- `charges_gas_limit()` is false for every variant in `BaseUpgrade::EXECUTION_VARIANTS`.
- EIP-8037 guard: `test_eip8037_state_gas_disabled_on_all_base_upgrades` still passes, so `spend_all` leaving the reservoir alone is safe.

In `eip8130.rs`: `billable_gas == gas_limit + payer_auth` under Zenith regardless of `call_gas_spent`, floor, and refunds; a self-pay EIP-8130 transaction settles at its declared budget; an EIP-8130 reverting arb whose call reverts settles at its declared budget and the sequencer receives the full tip, while Everest bills the metered revert (S13); the estimate excludes `payer_auth` under Zenith so a sponsored transaction at `gas_limit = estimate` settles `estimate + payer_auth` once; Everest unchanged (S8).

In `builder/core/src/execution.rs`: with `tx_gas_limit` set, a declared limit above the cap is rejected with the permanent `MaxGasUsageExceeded` before execution, a limit at the cap is admitted, `payer_auth` counts toward the cap, and with `tx_gas_limit` unset nothing is checked before execution (S14).

### Tier 2: block executor test

In `block_executor.rs`, execute a block under Zenith containing plain transfers with declared limits of 100,000 and 250,000:

- Header `gasUsed == 350_000` and equals the last receipt's `cumulativeGasUsed` (S2, S3).
- The same block under Everest reports `42_000` (S12).

To add: a block that also carries the L1 attributes deposit, asserting header `gasUsed` is the deposit's metered gas plus the user transactions' limits (S3, S9), and vault balance deltas (S4).

### Tier 3: system tests

In `etc/systems/tests`, using `SystemTestStackBuilder::with_base_zenith_activation_block` as `builder_cutover.rs` does, with a sibling run without Zenith:

1. **Fee and receipt invariants.** Fund an account, send a plain transfer with `gas_limit = 1_000_000` and a non-zero tip. Assert S1 from `eth_getBalance` before and after (using `l1Fee` and `operatorFee` from the Base receipt), S2 and S3 from the receipt and `eth_getBlockByNumber`, and S6 from `eth_estimateGas` on the same call (expect `21_000`, allow the EIP-7623 floor). Compare `debug_traceTransaction` per-opcode gas with the pre-Zenith run (S11).
2. **The reverting arb pays.** Submit, in the same block, the two transactions from the [worked example](#what-charging-the-limit-does): a regular transaction (`tip = t`, `gas_limit` close to its `gas_used`) and a padded reverting arb (`tip = 20t`, `gas_limit` 80x its `gas_used`, reverts after its check). Assert the reverting arb is ordered first in both runs, that its sequencer payment under Zenith is `20t * gas_limit` (80x its own pre-Zenith payment, about 67x the regular transaction's with the example's numbers), and that under Everest it paid about the same as the regular transaction (S13).
3. **Reverting-arb flood against the predicate budget.** From many accounts, submit enough high-tip validity transactions whose predicates match and whose calls revert to exhaust `predicate_eval_hard_cutoff` (set it low in the test builder config), plus lower-tip validity transactions whose predicates match and whose calls succeed. Under Everest, assert the builder reports `predicate_eval_budget_exhausted` deferrals, the regular transactions are deferred to a later flashblock, and each included reverting arb paid metered gas. Under Zenith, assert each included reverting arb paid its full declared limit at its tip, so the flood's cost per flashblock equals the executions it was sized for (S15). This is the test that pins the problem the rule is for; the behavioural change (fewer reverting arbs submitted) is economic and is measured on mainnet, not in a devnet.
4. **Reservation under adversarial load.** Flood the devnet with transactions that declare the EIP-7825 cap (`2^24` gas) and exit after about 25,000 gas. Assert each pays `2^24 * effectiveGasPrice`, that a block's header `gasUsed` reaches the block gas limit while metered execution stays near zero, that the number of such transactions per block is bounded by `block_gas_limit / 2^24`, and that the base fee rises across consecutive blocks per EIP-1559. Under Everest the same flood fits many more transactions per block and pays a fraction of the price (S13).
5. **Builder and validator agreement.** Run the builder and client together across activation and assert the client follows every builder block (the `smoke_test_builder_and_client_block_sync` pattern); run the sync-parity fuzz profile with Zenith enabled (S7).
6. **Invalid payload rejection.** Submit a `newPayload` whose header `gasUsed` is the metered sum for a block after activation and assert the client returns `INVALID` (the `invalid_new_payload.rs` pattern) (S7).
7. **EIP-8130.** Run the existing `eip8130.rs` system test with Zenith and assert the payer debit is `(gas_limit + payer_auth) * effective_gas_price + l1Fee + operatorFee` (S8).
8. **Proving.** Run the in-process proof test on a Zenith block containing the transfer above (S10).
9. **Zenith off.** Run the same suite without Zenith; every assertion about metered gas must hold as today (S12).

### Tier 4: full suite

`just test` and `just devnet tests` with no changes to existing assertions (S12). Any existing test that needs its assertion changed is a sign the gate leaked into the pre-activation path and must be investigated, not updated. The one intended exception is the EIP-8130 Keystore tests that already execute under Zenith and therefore see gas-limit-priced settlement; their assertions distinguish the metered estimate from the settled charge.

### Demo

Two scripts, both run from the repository root against the Docker devnet.

**End-to-end: the reverting-arb flood.** [`etc/scripts/devnet/gas-limit-pricing-demo.sh`](../../etc/scripts/devnet/gas-limit-pricing-demo.sh) reproduces the problem and shows the rule closing it on one chain, before and after Zenith activates:

```sh
L2_BUILDER_PREDICATE_EVAL_HARD_CUTOFF_MS=2 L2_BASE_ZENITH_BLOCK=3000 just devnet up-single zenith
./etc/scripts/devnet/gas-limit-pricing-demo.sh
```

It drives `just load-test gas-limit-pricing` (see [`crates/infra/load-tests`](../../crates/infra/load-tests/README.md)) twice. The workload is 200 validity-transaction senders, six transactions in flight each, every transaction carrying 63 always-true storage predicates plus the required `block_number` expiry bound (the 64-predicate cap) so each one costs the builder a full predicate evaluation (about 0.05 ms on the devnet). Ten percent of the senders are *reverting arbs*: a 20x priority fee, a declared limit of 500,000 gas sized for the arbitrage they hope to run, and a call to `DoubleCounter` that reverts after about 21,000 gas (an unknown selector). The rest are *regular transactions*: the market tip and `StorageFiller.fillStorage` writing four fresh slots, which uses about 114,000 of its 122,000 declared gas, the shape of an arbitrage that actually executes. With a 2 ms predicate budget the builder evaluates about 40 candidates per 200 ms block: the reverting arbs first, then a few regular transactions, and defers the rest. The first run finishes before the activation block (metered billing), the second after it (gas limit pricing). For each phase the script reads the per-cohort receipts from the load tester's summary (`LOAD_TEST_OUTPUT`) and the builder's Prometheus counters before and after, and prints:

- per cohort: confirmed, reverted, gas used, gas declared, the tip paid to the sequencer and the total fee (per transaction), and inclusion latency p50 / p95 / max;
- submitted vs included, so transactions that expired unevaluated show up as never included;
- builder: validity candidates evaluated and deferred, how many deferrals were `budget_exhausted`, how many block builds hit the predicate cutoff, validity transactions included, validity gas billed, and validity-flow priority-fee revenue to the sequencer.

What it shows, from a run on the Docker devnet (2 ms cutoff, 60 s per phase, exact signed tips from the load tester, effective prices from receipts):

| | Everest (metered) | Zenith (gas limit priced) |
|---|---|---|
| Block builds that hit the predicate cutoff | 321 | 325 |
| Validity candidates evaluated / deferred `budget_exhausted` | 14,693 / 34,824 | 14,326 / 37,772 |
| Reverting-arb inclusion latency p50 / p95 / max | 0.33 s / 0.40 s / 0.60 s | 0.52 s / 1.52 s / 2.00 s |
| Regular inclusion latency p50 / p95 / max | 2.53 s / 6.40 s / 31.4 s | 3.80 s / 8.10 s / 32.0 s |
| Regular billed gas per tx (declared 122,000) | 114,140 (94%) | 122,000 |
| Reverting-arb billed gas per tx (declared 500,000) | 21,231 (4%) | 500,000 |
| Reverting-arb tip to the sequencer per tx | 0.0000530 ETH | 0.0012903 ETH (24.3x) |
| Regular tip per tx | 0.0000142 ETH | 0.0000157 ETH |
| Reverting-arb tip / regular tip | 3.7x, for a 20x rank, using 0.19x the gas | 82x (20x tip × 4.1x declared gas) |
| Validity-flow priority-fee revenue to the sequencer | 0.251 ETH | 1.342 ETH (5.3x) |
| Validity transactions included in 60 s | 14,647 | 11,938 |

The problem is the first four rows: the reverting arbs sort first, the budget is exhausted in every block, they land in the next block, and the regular transactions, the ones that use the gas they declared and pay for it, wait a median of 2.5 s and up to 31 s (150 blocks) behind them. At this load every regular transaction still landed inside its 48 s expiry, so the devnet shows the delay rather than the exclusion; on mainnet, where the sequencer team measured ~14,000 candidates per flashblock against the 10 ms budget, the same ordering means the regular transactions are not evaluated at all, and an arbitrage that executes 30 s late is usually one that reverts. The fix is the next rows: the reverting arb is billed its declared 500,000 instead of 21,231, so its tip to the sequencer rises 24x and is 82x a regular transaction's tip, which is what a 20x bid on 4x the declared gas should cost, while the regular transaction's bill barely moves because it already used what it declared. The total-fee ratio moves less (10.6x) because the devnet base fee, about 1.3 gwei, dominates both cohorts' effective price. Deferral counts are similar in both phases because the load generator does not react to prices; the last row shows the other effect of the rule, blocks filling by declared gas (the devnet block gas limit is 6,000,000). The script asserts that under Zenith every cohort's billed gas equals its declared gas, that reverting arbs reverted, that a reverting arb's tip is at least half the expected `20 * arb_limit / regular_limit` multiple of a regular transaction's, and that the reverting arb's tip rose at least 10x from the metered phase; it exits non-zero otherwise.

The devnet runs 200 ms blocks from Denim (block 25), so `L2_BASE_ZENITH_BLOCK=3000` is about ten minutes out; the metered phase needs about three minutes (load-tester build, funding, 60 s of load, drain) and the script measures the block time and skips that phase if there is not enough room, running the Zenith phase alone. `L2_BUILDER_PREDICATE_EVAL_HARD_CUTOFF_MS` is passed to the single-mode builder by `etc/docker/docker-compose.yml`; at the production default of 10 ms a 200-sender flood on a laptop does not reach the cutoff, and `just load-test validity-stress` scale is needed to show the deferrals. The devnet block gas limit is 6,000,000, which is why the reverting arb limit is 500,000 rather than the 2,000,000 of the worked example: under Zenith blocks fill by declared gas, and a 2,000,000 reverting arb would leave room for two per block. If the host sleeps during the wait for activation the sequencer races to catch up afterwards and the 48 s expiry bound passes in seconds; `REUSE_EVEREST=1` reruns only the Zenith phase against a kept `OUT_DIR` once the chain is back at wall-clock.

**Settlement check.** [`etc/scripts/devnet/gas-limit-pricing-settlement-check.sh`](../../etc/scripts/devnet/gas-limit-pricing-settlement-check.sh) is the small exact-arithmetic check of the rule itself, with no load tester: it sends `TX_COUNT` plain transfers from a prefunded account, each declaring `GAS_LIMIT` gas (default 1,000,000) for a call that needs 21,000, and asserts that every receipt's `gasUsed` equals the declared limit (S2); that the sender's balance delta equals `sum(value + gasUsed * effectiveGasPrice + l1Fee + operatorFee)` exactly, with the operator fee computed from the receipt's `operatorFeeScalar` and `operatorFeeConstant` (S1); that `eth_estimateGas` returns the metered 21,000 (S6); and that each touched block's header `gasUsed` equals its user transactions' declared limits plus its deposits' metered gas (S3). It also prints what the transfers would have cost at metered gas. Run it after block 100 on `just devnet up-single zenith`; against a devnet without Zenith it shows the pre-activation behaviour and fails.

### Mainnet data scan

Not a correctness test, but part of validating the design: scan a recent range of Base Mainnet blocks (a `base-bench` subcommand in [`etc/systems`](../../etc/systems), next to the DA block-range measurement, or a one-off script over `eth_getBlockReceipts`) and report, per transaction type, per tip percentile, and per top contract:

- the distribution of `gas_limit / gas_used`, and how concentrated low utilisation is in the top tip percentiles, which measures how much of today's ordering is bought with gas that is not used;
- the fee multiplier senders would have paid, which sizes the user communication;
- the share of blocks whose reserved gas exceeds the EIP-1559 target, and the resulting base-fee trajectory, which informs any target change in the specification phase;
- how often an `eth_estimateGas`-sized limit would have run out of gas, which bounds how far limits can tighten. Receipts cannot answer this: post-refund `gasUsed` is not the minimum feasible limit (EIP-150 forwarding and gas-dependent code paths break that inference). This item needs `eth_estimateGas` against an archive node at the parent state of a sample of transactions, compared with the limit they actually declared.

Except for the last item, the scan needs no re-execution: `gasLimit` and `gasUsed` are in the transactions and receipts.

## Risks and open questions

- **Fee shock.** Users with 2x to 5x padded limits see 2x to 5x execution fees. Mitigations: measurement first, wallet outreach, and an announcement window. `--builder.max_gas_per_txn`, now a cap on declared limits, bounds the worst case below EIP-7825's `2^24`.
- **Estimation becomes load-bearing.** `eth_estimateGas` results that are too low turn into out-of-gas failures that pay the full limit; results that are high are paid in full. Calls whose gas depends on state at inclusion time (swaps, auctions) cannot be estimated exactly, so some padding remains rational and is now a visible cost. The data scan should report how often the optimistic estimate would have failed.
- **Legitimate searchers pay for uncertainty.** A searcher whose opportunity may disappear before inclusion pays for the success-path limit either way. This is the intended incentive (check predicates cheaply, bound the limit), but it is a real cost shift from the sequencer to searchers and should be communicated as such.
- **Static EIP-8130 coinbase tips are ranked on an approximate denominator.** `UnifiedTipOrdering` ranks a static coinbase tip by `coinbase_tip / gas_limit`, but the transaction reserves and is billed `gas_limit + payer_auth`, any EIP-1559 priority fee it also pays is not in the key, and the builder only counts the coinbase transfer as revenue once phase 0 commits. For these transactions the rule prices reservation correctly but the ranking key is not exactly revenue per reserved gas. Whether the pool should divide by `gas_limit + payer_auth` and add the priority fee is a txpool follow-up, not part of this POC.
- **Semantics of `gasUsed`.** The field changes meaning for user transactions for a widely used audience. Explorer and indexer owners should review before the rule is specified, and the metered gas should remain reachable through traces.
- **Base-fee transition.** Reserved gas is higher than metered gas, so the base fee steps up after activation until limits tighten. Decide, from the data scan, whether to change the target or elasticity alongside activation or accept the transition.
- **Fewer transactions per block at first.** Admission and accounting now agree, so a block holds at most `block_gas_limit` of declared gas instead of admitting more as long as metered gas fits. Until limits tighten, blocks carry fewer transactions. The data scan's utilisation distribution sizes this.
- **EIP-8130 payer authentication.** Charging `payer_auth` at the full reserved amount is consistent with reservation but differs from today's floor-versus-standard-rate settlement, and the estimate no longer includes it, so `eth_estimateGas * gasPrice` understates a sponsored transaction's total by `payer_auth * gasPrice` (inherent to metering payer authentication on top of `gas_limit`). Confirm this matches the intended EIP-8130 fee model.
- **EIP-8130 and `disable_fee_charge`.** `Eip8130Executor::execute` settles fees regardless of `cfg.disable_fee_charge` (pre-existing), so a signed execute-mode EIP-8130 transaction run under that flag reports the reserved budget rather than metered gas. EIP-8130 estimation goes through `simulate`, which is unaffected, so normal RPC paths are fine; the exemption is by execution mode, not by the flag.
- **Divergence from the OP Stack fee model.** Base-specific execution rules already exist (Azul onward), but this rule needs a specification entry, and shared tooling that assumes `gasUsed < gasLimit` for successful calls must be checked.

## Rollout

1. Implement the POC behind `BaseSpecId::charges_gas_limit()` (Zenith), with the unit, block-executor, and system tests above. No change to Base Sepolia or Base Mainnet configs.
2. Run the mainnet data scan and publish the utilisation, fee-multiplier, and base-fee results.
3. Review `gasUsed` semantics with explorer and wallet stakeholders; communicate the cost shift to searchers and wallet vendors.
4. Write the specification and move the gate to a named upgrade per [UPGRADES.md](UPGRADES.md). Schedule on Base Sepolia, observe estimation behavior and failure rates, then Base Mainnet.

## References

- [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559): fee market and base-fee update from header `gasUsed`.
- [EIP-3529](https://eips.ethereum.org/EIPS/eip-3529): refund cap (`gas_used / 5`).
- [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623): calldata floor.
- [EIP-7778](https://eips.ethereum.org/EIPS/eip-7778): block gas accounting without refunds (L1 precedent for counting reserved rather than net gas).
- [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825): transaction gas limit cap.
- [EIP-8130](https://eips.ethereum.org/EIPS/eip-8130): Base transactions.
- Code: [`handler.rs`](../../crates/common/evm/src/handler.rs), [`eip8130.rs`](../../crates/common/evm/src/eip8130.rs), [`l1block.rs`](../../crates/common/evm/src/l1block.rs), [`block_executor.rs`](../../crates/common/evm/src/executor/block_executor.rs), [`basefee.rs`](../../crates/execution/chainspec/src/basefee.rs), [`ordering.rs`](../../crates/execution/txpool/src/ordering.rs), [`builder execution.rs`](../../crates/builder/core/src/execution.rs), [`UPGRADES.md`](UPGRADES.md), [`TESTING.md`](TESTING.md).
