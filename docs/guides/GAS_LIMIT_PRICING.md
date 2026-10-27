# Gas Limit Pricing

This document covers gas limit pricing: charging a Base transaction for the gas it reserves (its declared `gas_limit`) instead of the gas it consumes (metered `gas_used`). The purpose is fair and transparent transaction ordering. The sequencer ranks transactions by priority fee per gas, but a transaction pays that fee only on the gas it ends up using, so a sender can buy a top rank with a high per-gas bid on a large limit and then use almost none of it. Charging the limit makes the bid and the payment the same quantity, which makes that trick cost what it claims and makes spam expensive. The doc describes the problem, the rule, the design decisions, and a proof of concept (POC) gated on the Zenith experimental upgrade so the change can be exercised on a devnet before it is specified as a real network upgrade.

## Contents

- [Summary](#summary)
- [Problem: ordering is by a bid nobody has to pay](#problem-ordering-is-by-a-bid-nobody-has-to-pay)
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

- **Billable gas is the declared gas limit.** Every non-system fee component that is priced per gas (EIP-1559 base fee, priority fee, operator fee) is charged on `gas_limit`, with no post-execution reimbursement. The L1 data fee is unchanged.
- **Ordering is unchanged.** The pool and builder keep ranking by priority fee per gas. The rule does not change how transactions are ranked; it changes what a rank costs, so that the ranking key (`tip` per gas) times the reserved gas is exactly what the sender pays.
- **`gasUsed` reports billable gas.** Receipt `gasUsed`, `cumulativeGasUsed`, and header `gasUsed` count declared gas limits for user transactions, so `fee = gasUsed * effectiveGasPrice + l1Fee + operatorFee` still holds and the block fills by the capacity the builder reserved and the senders paid for.
- **Deposits are untouched.** Deposits are not ordered by tip and pay no L2 execution fee, so they keep metered gas and their Regolith receipt shape.
- **No partial rate.** The full limit is charged. Any fraction leaves the discount, and the attack, in place at a smaller scale.
- **Base fee target and elasticity are unchanged in the POC.** Header `gasUsed` becomes reserved gas, so the base fee reacts to reserved demand; the mainnet data scan quantifies the shift before any parameter change is decided.
- **Activation gate is `BaseUpgrade::Zenith`.** Genesis-only, devnet and system tests only. Not scheduled on Base Sepolia or Base Mainnet.

The POC touches two consensus sites in `base-common-evm` (a hook in `BaseHandler::refund` and a branch in `Eip8130Executor::billable_gas`) and moves one builder check (`--builder.max_gas_per_txn`) in front of execution. RPC simulation keeps metered gas because it runs with fee charging disabled.

## Problem: ordering is by a bid nobody has to pay

### How ordering works today

A sender signs three gas-related fields into a standard transaction: `gas_limit` (the most gas the transaction may consume), `max_fee_per_gas` (the ceiling on the total price per gas, base fee plus tip), and `max_priority_fee_per_gas` (the sequencer's cut per gas; a legacy transaction has a single `gas_price` instead). None of them says how much gas the transaction will use; that is only known after it executes.

The pool ranks transactions before anything executes, from those signed fields and the current base fee:

```text
effective_tip_per_gas = min(max_fee_per_gas - base_fee, max_priority_fee_per_gas)   // EIP-1559
                      = gas_price - base_fee                                          // legacy
                      = not includable, if max_fee_per_gas < base_fee
```

[`UnifiedTipOrdering::priority`](../../crates/execution/txpool/src/ordering.rs) uses this rate as the ranking key for standard transactions (stored as `tip / 1`, so `gas_limit` does not enter the comparison) and `coinbase_tip / gas_limit` for statically decodable EIP-8130 coinbase tips; ties go to fewer validity predicates. The builder then pulls transactions from the pool in that order, admits each one if its declared `gas_limit (+ payer_auth)` fits the remaining block gas, executes it, and only then learns `gas_used`. In normal conditions `max_fee_per_gas` sits well above the base fee, so `max_priority_fee_per_gas` is the effective tip and is the knob that buys position.

After execution the sender pays `effective_tip_per_gas * gas_used` to the sequencer and `base_fee * gas_used` to the `BaseFeeVault` (plus the L1 data fee and operator fee), and the pre-charged `gas_limit - gas_used` is refunded. Rank and payment are therefore decided at different times from different quantities:

```text
rank    = effective_tip_per_gas              known when the transaction arrives
payment = effective_tip_per_gas * gas_used   known after it executes
```

### How a sender games it

Because the tip is only paid on `gas_used`, and `gas_limit` neither affects the rank nor costs anything unless it is used, a sender can buy the top of the block cheaply and keep the option to do real work from there:

1. Set `max_priority_fee_per_gas` far above the market tip, say 20x. The pool ranks the transaction first; nothing about `gas_limit` or the eventual `gas_used` affects that.
2. Declare a `gas_limit` sized for the full success path (say 500,000, or anything up to the EIP-7825 cap of `2^24`). The limit plays no part in the rank; what it buys is the option to execute the whole opportunity from the top slot if it is there. Today that option is free: the sender only needs `gas_limit * max_fee_per_gas` to be affordable as a temporary hold, and the unused part is refunded after execution.
3. Make the transaction exit early in the common case: check a predicate, and `STOP` or revert when it fails. `gas_used` is a few tens of thousands.
4. The bill is `20x tip * ~25,000 gas`, about what an honest transaction pays for `1x tip * 500,000 gas`, for a position ahead of every honest transaction. Repeat every block.

The honest transaction bidding for that position has to raise its own tip, and it pays that tip on all of the gas it really uses. The builder cannot filter the probe, because the early exit is only visible after executing it. Searchers do a milder version of this unintentionally with loose predicate bounds and a limit sized for the success path; a spammer does it on purpose, with many such transactions, to hold the top of the block for a fraction of what the position is worth.

A probe that always exits early and declares a tight limit is not the problem: it ranks first too, but it takes only the 25,000 gas it pays for and captures nothing. The harm comes from the combination of a cheap rank and a free option on a large amount of gas, and the rule removes the free option rather than the rank.

### Why it is a problem for the chain

The gas the pool and the builder reason about is the declared `gas_limit`: the builder admits a transaction only if `cumulative_gas_used + gas_limit (+ payer_auth) <= block_gas_limit` ([`ExecutionInfo::is_tx_over_limits`](../../crates/builder/core/src/execution.rs)), and the pool's balance check requires the sender to be able to pay `gas_limit * max_fee_per_gas`. The fee is settled on `gas_used`. Everything below follows from that mismatch.

Worked example at a base fee of 0.01 gwei and a market tip of 1 gwei:

| | Honest arbitrage | Padded probe |
|---|---|---|
| `gas_limit` | 600,000 | 2,000,000 |
| `gas_used` | 500,000 | 25,000 |
| `tip` (priority fee per gas) | 1 gwei | 20 gwei |
| Rank | second | first |
| Pays the sequencer today (`tip * gas_used`) | 0.0005 ETH | 0.0005 ETH |
| Pays the sequencer under gas limit pricing (`tip * gas_limit`) | 0.0006 ETH | 0.04 ETH |

Today the probe buys a 20x higher rank for the same money. Under gas limit pricing the probe pays 80x what it paid before and its rank costs 67x what the honest transaction pays; a probe that wants to be cheap has to declare a tight limit, which caps the work it can do if the opportunity is there.

The consequences for the chain:

- **Ordering is not what it says.** The top of the block goes to whoever is willing to bid per gas on gas they will not use, not to whoever pays the most for the space.
- **Optionality is free.** A sender can hold the right to execute a large amount of gas from the top of every block and pay only when the opportunity materialises; everyone else's bids are set against that free option.
- **Builder work is wasted.** The builder admits and executes high-`tip` transactions first. When they exit early it has spent time on them for a fraction of the revenue the bid implied, and only learns that after executing. It cannot filter them beforehand, because `gas_used` is not knowable before execution.
- **Admission and payment disagree.** Admission reserves `gas_limit` and accounting then adds only `gas_used`, so the reservation is released as soon as the transaction executes. The builder can neither plan on declared limits nor charge for them; the declared gas is a claim on capacity that is never paid for and never honoured.

The common cause is that the pool and the builder reason about `gas_limit` and the fee is settled on `gas_used`. Charging `gas_limit` removes the gap.

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

- **Fair ordering.** For standard transactions, ranking by `tip` is exactly ranking by protocol tip revenue per unit of reserved block space, because that revenue is `tip * gas_limit`. A transaction cannot outrank another while paying less per unit of the space it reserved; the only way to rank higher is to pay more per gas on all the gas you declare. Static EIP-8130 coinbase tips are ranked by `coinbase_tip / gas_limit` while the reservation is `gas_limit + payer_auth` and settlement also credits the priority fee, so for them the ranking key is an approximation of revenue per reserved gas rather than exactly it; see [Risks](#risks-and-open-questions).
- **Transparent ordering.** A sender can compute what any position in the block costs from public information (`tip`, `gas_limit`, the base fee) before submitting. Nothing about the price depends on how the transaction runs.
- **Spam and probing are priced.** A padded probe pays `gas_limit / gas_used` times more than today (80x in the example above, where its rank also ends up costing 67x what the honest transaction pays). Reserving the block's admission budget costs the full price of that budget. Reverted and early-exit transactions pay the same as successful ones for the same declared limit.
- **Searchers adapt by tightening.** The rational response is a limit sized for the real path and predicates checked as cheaply and early as possible, or off-chain. Loose bounds are still allowed; they just cost what they reserve. Searchers already optimize gas; the limit becomes one more thing to optimize.
- **Builder effort tracks revenue.** The builder knows a transaction's tip revenue (`tip * B`) before executing it, so the high-`tip` transactions it executes first are the ones that pay the most per unit of the capacity they take. Early exits no longer convert admitted capacity into a refund, and the per-transaction gas cap can be decided before execution ([Change 3](#change-3-builder-per-transaction-gas-cap)). `gas_headroom` metrics become exact because admission and accounting agree. The rule prices reservation; it does not claim to order transactions by total payment or by execution effort.
- **Capacity is paid for.** Header `gasUsed` counts reserved gas, so a block full by reservation reads as full to EIP-1559 whenever execution gas is the binding term of the Jovian base-fee input (`max(gasUsed, blobGasUsed)`). The base fee then responds to the demand for block space that was actually allocated, and senders are paid back for tightening limits with more transactions per block.

## Alternatives considered

| Alternative | Rule | Why not |
|---|---|---|
| Rank by total bid, `tip * gas_limit` | Keep metered payment; order by the product. | Ranks a large transaction with a low per-gas tip above small ones, so it is not a per-gas market any more; and the padded probe still pays `tip * gas_used`, so reserving capacity stays cheap. The ranking changes but the payment gap remains. |
| Rank by realised payment, `tip * gas_used` | Execute, then order by what was paid. | Requires executing every candidate before ordering it, which is the builder cost this change wants to remove, and the probe still pays little for the space it reserved. Also makes ordering depend on execution, so a sender cannot know its position's price before submitting. |
| Penalise reverts | Charge the limit only when the transaction fails. | A probe that exits early with `STOP` is a successful transaction. The attack does not need a revert. |
| Pool heuristics on `gas_limit / gas_used` | Track senders' past utilisation and deprioritise padders. | Gameable with fresh addresses; punishes honest senders whose gas is state-dependent; and adds per-sender state to the pool's hot path. Does not change what anyone pays. |
| Partial charge | Bill `gas_used + p * (gas_limit - gas_used)`. | For any `p < 1` the probe still pays less per unit of rank than the honest transaction, by a factor of about `1/p`; the gap shrinks but stays. Could inform a transition if the data scan shows a large shock, at the cost of the invariants above. |
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

**Searchers and bots.** The cost of a position in the block is `tip * gas_limit`, computable before submission. Probes and loosely bounded transactions pay for the gas they declare, so limits should be sized for the real path and predicates checked early or off-chain. A high `tip` on a padded limit no longer buys cheap priority.

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
| S13 | Ordering is revenue-exact for standard transactions: for any two standard user transactions in a block, the one ranked higher by the pool paid the sequencer at least as much tip per unit of reserved gas, and a padded transaction pays `gasLimit / gasUsed` times what it would pay at metered gas. | Builder test, adversarial devnet run |
| S14 | A transaction whose reserved gas exceeds `--builder.max_gas_per_txn` is rejected before execution under Zenith and after execution otherwise. | Unit (builder) |

### Tier 1: unit tests

In `handler.rs` (`#[cfg(test)] mod tests`), executing single transactions against an `InMemoryDB` with a funded caller and reading the caller, beneficiary, and `BaseFeeVault` balances afterwards:

- Zenith, plain transfer with `gas_limit = 100_000`: `tx_gas_used == 100_000`; caller debited `100_000 * effective_gas_price`; beneficiary credited `100_000 * tip`; `BaseFeeVault` credited `100_000 * basefee` (S1, S2, S4).
- Zenith with an SSTORE clear that earns an EIP-3529 refund: refund is zero and the settlement is the same as the plain transfer (S5).
- Zenith, out of gas: identical settlement to Everest, since out of gas already bills the limit (S11).
- Zenith, `disable_fee_charge`: `tx_gas_used == 21_000`, nothing debited (S6).
- Zenith deposit (`source_hash` set): `tx_gas_used == 21_000` as on Everest; nothing debited, nothing credited (S9).
- Everest: the plain transfer is billed 21,000; `test_consume_gas`, `test_revert_gas`, `test_consume_gas_with_refund`, and the rest of the existing module pass unchanged (S12).
- `charges_gas_limit()` is false for every variant in `BaseUpgrade::EXECUTION_VARIANTS`.
- EIP-8037 guard: `test_eip8037_state_gas_disabled_on_all_base_upgrades` still passes, so `spend_all` leaving the reservoir alone is safe.

In `eip8130.rs`: `billable_gas == gas_limit + payer_auth` under Zenith regardless of `call_gas_spent`, floor, and refunds; a self-pay EIP-8130 transaction settles at its declared budget; the estimate excludes `payer_auth` under Zenith so a sponsored transaction at `gas_limit = estimate` settles `estimate + payer_auth` once; Everest unchanged (S8).

In `builder/core/src/execution.rs`: with `tx_gas_limit` set, a declared limit above the cap is rejected with the permanent `MaxGasUsageExceeded` before execution, a limit at the cap is admitted, `payer_auth` counts toward the cap, and with `tx_gas_limit` unset nothing is checked before execution (S14).

### Tier 2: block executor test

In `block_executor.rs`, execute a block under Zenith containing plain transfers with declared limits of 100,000 and 250,000:

- Header `gasUsed == 350_000` and equals the last receipt's `cumulativeGasUsed` (S2, S3).
- The same block under Everest reports `42_000` (S12).

To add: a block that also carries the L1 attributes deposit, asserting header `gasUsed` is the deposit's metered gas plus the user transactions' limits (S3, S9), and vault balance deltas (S4).

### Tier 3: system tests

In `etc/systems/tests`, using `SystemTestStackBuilder::with_base_zenith_activation_block` as `builder_cutover.rs` does, with a sibling run without Zenith:

1. **Fee and receipt invariants.** Fund an account, send a plain transfer with `gas_limit = 1_000_000` and a non-zero tip. Assert S1 from `eth_getBalance` before and after (using `l1Fee` and `operatorFee` from the Base receipt), S2 and S3 from the receipt and `eth_getBlockByNumber`, and S6 from `eth_estimateGas` on the same call (expect `21_000`, allow the EIP-7623 floor). Compare `debug_traceTransaction` per-opcode gas with the pre-Zenith run (S11).
2. **Ordering is paid for.** Submit, in the same block, the two transactions from the [worked example](#problem-ordering-is-by-a-bid-nobody-has-to-pay): an honest transaction (`tip = t`, `gas_limit` close to its `gas_used`) and a padded probe (`tip = 20t`, `gas_limit` 80x its `gas_used`, exits with `STOP`). Assert the probe is ordered first in both runs, that its sequencer payment under Zenith is `20t * gas_limit` (80x its own pre-Zenith payment, about 67x the honest transaction's with the example's numbers), and that under Everest it paid about the same as the honest transaction (S13).
3. **Reservation under adversarial load.** Flood the devnet with transactions that declare the EIP-7825 cap (`2^24` gas) and exit after about 25,000 gas. Assert each pays `2^24 * effectiveGasPrice`, that a block's header `gasUsed` reaches the block gas limit while metered execution stays near zero, that the number of such transactions per block is bounded by `block_gas_limit / 2^24`, and that the base fee rises across consecutive blocks per EIP-1559. Under Everest the same flood fits many more transactions per block and pays a fraction of the price (S13).
4. **Builder and validator agreement.** Run the builder and client together across activation and assert the client follows every builder block (the `smoke_test_builder_and_client_block_sync` pattern); run the sync-parity fuzz profile with Zenith enabled (S7).
5. **Invalid payload rejection.** Submit a `newPayload` whose header `gasUsed` is the metered sum for a block after activation and assert the client returns `INVALID` (the `invalid_new_payload.rs` pattern) (S7).
6. **EIP-8130.** Run the existing `eip8130.rs` system test with Zenith and assert the payer debit is `(gas_limit + payer_auth) * effective_gas_price + l1Fee + operatorFee` (S8).
7. **Proving.** Run the in-process proof test on a Zenith block containing the transfer above (S10).
8. **Zenith off.** Run the same suite without Zenith; every assertion about metered gas must hold as today (S12).

### Tier 4: full suite

`just test` and `just devnet tests` with no changes to existing assertions (S12). Any existing test that needs its assertion changed is a sign the gate leaked into the pre-activation path and must be investigated, not updated. The one intended exception is the EIP-8130 Keystore tests that already execute under Zenith and therefore see gas-limit-priced settlement; their assertions distinguish the metered estimate from the settled charge.

### Demo

[`etc/scripts/devnet/gas-limit-pricing-demo.sh`](../../etc/scripts/devnet/gas-limit-pricing-demo.sh) shows the rule on a local devnet with the padded-transaction shape. Start a devnet with Zenith scheduled and run the script once the chain passes the activation block (100 by default):

```sh
just devnet up-single zenith
./etc/scripts/devnet/gas-limit-pricing-demo.sh            # waits for block 100, then runs
TX_COUNT=10 GAS_LIMIT=5000000 ./etc/scripts/devnet/gas-limit-pricing-demo.sh
```

It sends `TX_COUNT` plain transfers from a prefunded devnet account, each declaring `GAS_LIMIT` gas (default 1,000,000) for a call that needs 21,000, then prints per-transaction receipts and asserts (the script exits non-zero if any fails): every receipt's `gasUsed` equals the declared limit (S2); the sender's balance delta equals `sum(value + gasUsed * effectiveGasPrice + l1Fee + operatorFee)` exactly, with the operator fee computed from the receipt's `operatorFeeScalar` and `operatorFeeConstant` (S1); `eth_estimateGas` returns the metered 21,000 (S6); and each touched block's header `gasUsed` equals the sum of its user transactions' declared limits plus its deposits' metered gas (S3). It also prints what the same transfers would have cost at metered gas and the resulting cost multiplier, which is the price padding now carries. Running it against a devnet without Zenith (`just devnet up-single`) shows the pre-activation behavior and fails: `gasUsed` of 21,000 and a balance delta to match.

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
- **Honest searchers pay for uncertainty.** A searcher whose opportunity may disappear before inclusion pays for the success-path limit either way. This is the intended incentive (check predicates cheaply, bound the limit), but it is a real cost shift from the sequencer to searchers and should be communicated as such.
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
