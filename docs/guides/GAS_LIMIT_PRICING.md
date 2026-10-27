# Gas Limit Pricing

This document covers gas limit pricing: charging a Base transaction for the gas it reserves (its `gas_limit`) instead of the gas it consumes (`gas_used`). The purpose is pipelined block agreement: validators agree on block N+1 while executing block N, which is only safe when a block's cost, capacity, and fees are known from its contents before it is executed. The doc describes the pipelining model and what it needs from the fee rule, the rule itself, the design decisions, and a proof of concept (POC) gated on the Zenith experimental upgrade so the change can be exercised on a devnet before it is specified as a real network upgrade.

## Contents

- [Summary](#summary)
- [Goal: pipelined block agreement](#goal-pipelined-block-agreement)
- [Why the current fee model blocks pipelining](#why-the-current-fee-model-blocks-pipelining)
- [Proposed rule](#proposed-rule)
- [What pipelining gets from the rule](#what-pipelining-gets-from-the-rule)
- [Alternatives considered](#alternatives-considered)
- [Proof of concept](#proof-of-concept)
  - [Activation](#activation)
  - [Change 1: standard and deposit transactions](#change-1-standard-and-deposit-transactions)
  - [Change 2: EIP-8130 transactions](#change-2-eip-8130-transactions)
  - [What does not change](#what-does-not-change)
- [Effects](#effects)
- [Validation plan](#validation-plan)
- [Risks and open questions](#risks-and-open-questions)
- [Rollout](#rollout)
- [References](#references)

## Summary

Decisions:

- **Billable gas is the declared gas limit.** Every non-system fee component that is priced per gas (EIP-1559 base fee, priority fee, operator fee) is charged on `gas_limit`, with no post-execution reimbursement. The L1 data fee is unchanged.
- **`gasUsed` reports billable gas.** Receipt `gasUsed`, `cumulativeGasUsed`, and header `gasUsed` equal the declared gas limits, so the header `gasUsed` and the next block's base fee are functions of the block body, computable before execution. The invariant `fee = gasUsed * effectiveGasPrice + l1Fee + operatorFee` holds.
- **Deposits count their gas limit but pay no L2 fee.** Deposit gas is prepaid on L1; the receipt and header still count `gas_limit` so the header stays execution-independent.
- **Transactions invalid at execution time make the block invalid.** The proposer pre-validates nonce and balance against the parent state and in-block reservations, as the builder does today. Execution-time semantics do not change.
- **No partial rate.** The full limit is charged.
- **Base fee target and elasticity are unchanged in the POC.** The mainnet data scan quantifies the shift; any target change is decided in the specification phase.
- **Activation gate is `BaseUpgrade::Zenith`.** Genesis-only, devnet and system tests only. Not scheduled on Base Sepolia or Base Mainnet.

The POC touches two consensus sites in `base-common-evm`: a hook in `BaseHandler::refund` and a branch in `Eip8130Executor::billable_gas`. RPC simulation keeps metered gas because it runs with fee charging disabled.

## Goal: pipelined block agreement

Today a block is agreed only after it is executed: the proposer executes transactions to produce the state root, receipts root, and `gasUsed`, and validators re-execute to verify them. Execution sits on the critical path of consensus.

The target model pipelines the two:

- **Consensus does not wait for execution.** Validators agree on block N+1 while block N is being executed. Agreement is over the block body and the header fields that can be derived from it; execution-dependent commitments (state root, receipts root, logs bloom, withdrawals root) are verified after execution, one block behind.
- **Block capacity is predictable upfront.** The proposer bounds the execution work it commits to by summing declared gas limits against the block gas limit. No transaction is simulated before agreement. Gas limit is the only upper bound on execution work the proposer has without executing.
- **Reserving capacity costs money.** Because capacity is allocated by declared limit, a transaction that reserves 1,000,000 gas and exits after 25,000 has consumed 1,000,000 gas of the block's budget. If it pays for 25,000, block capacity can be exhausted at a fraction of its price. Charging the reservation closes that gap and gives senders a reason to declare tight limits.

The expected benefit is throughput: execution of block N overlaps with agreement on block N+1 instead of serializing the two phases.

This document does not design the consensus protocol, the deferred verification of execution-dependent header fields, or validator failure handling. It specifies the fee rule and enumerates exactly what that rule guarantees to the pipelined design.

## Why the current fee model blocks pipelining

All consensus fee logic lives in `base-common-evm`, which is `no_std` and shared by the node, the builder, the prover, and the acceptance tests.

### Standard transactions (`BaseHandler`)

[`crates/common/evm/src/handler.rs`](../../crates/common/evm/src/handler.rs):

| Phase | Today | Depends on execution? |
|---|---|---|
| `validate_against_state_and_deduct_caller` | Checks the caller can afford `gas_limit * max_fee_per_gas`, then deducts `gas_limit * effective_gas_price` (revm `calculate_caller_fee`), the L1 data fee, and the operator fee computed on `gas_limit` (`L1BlockInfo::tx_cost`). | No. The pre-charge is already a gas-limit charge. |
| `refund` | revm `post_execution::refund`: adds the EIP-7702 refund and caps the refund at `gas_used / 5` (EIP-3529). | Yes. |
| `eip7623_check_gas_floor` | Raises spent gas to the calldata floor if execution used less. | Yes. |
| `reimburse_caller` | Returns `(remaining + refunded) * effective_gas_price` plus the operator fee refund (`L1BlockInfo::operator_fee_refund`: operator fee on `gas_limit` minus operator fee on `gas_used`). | Yes. |
| `reward_beneficiary` | Pays the sequencer `priority_fee * gas_used`, the `BaseFeeVault` `basefee * gas_used`, the `L1FeeVault` the L1 cost, and the `OperatorFeeVault` the operator fee on `gas_used`. | Yes. |
| `execution_result` | Builds `ResultGas`; `tx_gas_used = max(total_spent - refunded, floor)` becomes receipt `gasUsed`. | Yes. |

Everything after the pre-charge depends on metered gas. The amount a sender ends up paying, the amount the sequencer earns, and the receipt `gasUsed` are unknown until the transaction runs.

Note that receipt `gasUsed` is already a billing quantity rather than raw execution gas: it is post-refund and post-floor. Redefining it as `gas_limit` extends that semantics rather than inventing a new one. Before Regolith, deposit receipts reported `gasUsed = gasLimit`; Regolith switched them to metered gas. The rule here returns deposits to the Bedrock shape for the same reason Bedrock had it: the value is known without execution.

### EIP-8130 transactions (`Eip8130Executor`)

[`crates/common/evm/src/eip8130.rs`](../../crates/common/evm/src/eip8130.rs) has its own settlement. `settle_fees` computes `billable_gas = max(sender_intrinsic + call_gas_spent - capped_refund, sender_floor) + payer_auth`, charges the payer that amount at the effective price, routes base fee, tip, L1 cost, and operator fee to their recipients, and refunds the surplus of the worst-case pre-charge. `simulate` (the estimate path) uses the gross metered amount and is read-only.

### Block accounting

[`BaseBlockExecutor::commit_transaction`](../../crates/common/evm/src/executor/block_executor.rs) adds `result.tx_gas_used()` to the running `gas_used`, which becomes receipt `cumulativeGasUsed` and header `gasUsed`. The next base fee is computed from the parent header: [`compute_jovian_base_fee`](../../crates/execution/chainspec/src/basefee.rs) uses `max(parent.gas_used, parent.blob_gas_used)` with the Holocene/Jovian parameters from `extra_data`. Because `gas_used` is metered, the base fee of block N+1 is unknown until block N has executed, which is exactly the dependency pipelining must remove.

The builder already reserves by limit: [`ExecutionInfo::is_tx_over_limits`](../../crates/builder/core/src/execution.rs) admits a transaction only if `cumulative_gas_used + tx.gas_limit (+ payer_auth) <= block_gas_limit`, then after execution adds metered gas to `cumulative_gas_used`. Admission and accounting disagree by `gas_limit - gas_used` per transaction.

### RPC

reth's `eth_call` and `eth_estimateGas` set `cfg.disable_fee_charge = true` before executing. `eth_estimateGas` reads `res.result.tx_gas_used()` from an unconstrained execution to seed its binary search; if that value were `gas_limit`, the search would collapse and return the cap. The POC therefore couples gas limit pricing to fee charging being enabled. `eth_simulateV1` does not set the flag in the pinned reth (it only disables the nonce and base-fee checks when `validation` is false), so it observes the rule; see [Effects](#effects).

## Proposed rule

For every transaction in a block where gas limit pricing is active:

1. **Billable gas** `B = gas_limit` for standard transactions and deposits, and `B = gas_limit + payer_auth` for EIP-8130 transactions (the same quantity the builder reserves).
2. **Non-deposit settlement.** The caller pays `B * effective_gas_price`. No part of the EIP-1559 pre-charge is reimbursed. EIP-3529 refunds and the EIP-7623 floor have no effect on the fee (both are bounded by `gas_limit`). The sequencer receives `B * priority_fee_per_gas`; the `BaseFeeVault` receives `B * basefee`; the `OperatorFeeVault` receives the operator fee schedule (`L1FeeParams::operator_fee_charge_inner`) applied to `B`, with no post-execution refund. The L1 data fee is unchanged.
3. **Deposits** pay no L2 execution fee, as today.
4. **Receipts and header.** Receipt `gasUsed = B`; `cumulativeGasUsed` and header `gasUsed` are the sum of `B` over the block, for deposits and non-deposits alike.
5. **Unchanged:** transaction validity rules, intrinsic gas, nonce and balance checks, the EIP-7825 per-transaction gas cap, the opcode gas schedule, `GAS`, call stipends and the 63/64 rule, out-of-gas semantics, L1 data fee, the Jovian DA footprint, and batch submission.

Execution is still metered internally, so a transaction behaves identically; only what is settled and reported changes.

## What pipelining gets from the rule

With the rule in place, the following are pure functions of `(block body, parent header, L1 attributes of the block)` and need no execution:

| Quantity | Derivation | Notes |
|---|---|---|
| Header `gasUsed` | Sum of `gas_limit` (+ `payer_auth`) over all transactions, deposits included. | Equals the reserved capacity the proposer checked against `block_gas_limit`. |
| Header `blobGasUsed` (Jovian DA footprint) | Sum over non-deposit transaction byte sizes times the DA footprint scalar. | Already execution-independent today. |
| Base fee of block N+1 | `compute_jovian_base_fee(parent)` from the two fields above and `extra_data`. | Lets the proposer price block N+1 and lets senders know their effective price before block N executes. |
| Per-transaction debit | `B * effective_gas_price(basefee_N)` + L1 data fee (tx bytes, `L1Block` parameters) + operator fee (`B`, `L1Block` parameters). | `L1Block` parameters for block N are set by the L1 attributes deposit at the top of block N and are derived from the L1 origin, not from user-transaction execution. |
| Fee transfers | Sequencer tip, `BaseFeeVault`, `L1FeeVault`, `OperatorFeeVault` credits, each from the debit above. | Fee accounting for the block can be done before execution. |
| Upper bound on execution work | `gasUsed` (sum of limits). | The only bound available without simulating; gas limit pricing makes it an honest bound because it is what senders pay for. |

What remains execution-dependent, and is therefore the pipelined design's job to verify one block behind: `stateRoot`, `receiptsRoot`, `logsBloom`, `withdrawalsRoot` (storage root of `L2ToL1MessagePasser`, Isthmus onward), per-receipt `status` and logs, and the metered gas inside traces.

**Admission.** For "invalid at execution time means invalid block" to hold without simulation, the proposer must validate each transaction against the parent state plus the cumulative reservations of earlier transactions from the same sender in the block: nonce continuity, and `balance >= value + B * max_fee_per_gas + l1_fee + operator_fee(B)`. The transaction pool and builder already perform this check at admission. Because the debit is execution-independent, it cannot be invalidated by the transaction's own execution; it can only be invalidated by an earlier transaction in the same block lowering the sender's balance. See [Risks](#risks-and-open-questions) for the EIP-7702 and EIP-8130 cases where that is possible.

## Alternatives considered

| Alternative | Rule | Why not |
|---|---|---|
| Fee on `gas_limit`, `gasUsed` stays metered | Change only `reimburse_caller` and `reward_beneficiary`. | Header `gasUsed` still depends on execution, so the next base fee is not derivable before executing the block; the goal is not met. Also breaks `fee = gasUsed * effectiveGasPrice`, which explorers and wallets compute today. |
| Header = sum of limits, receipts metered | Set header `gasUsed` from declared limits but keep receipt `gasUsed` metered. | Receipts no longer sum to the header, so the receipts root and header disagree in a way no client expects. Two notions of `gasUsed` in one block. |
| Partial charge | Bill `gas_used + p * (gas_limit - gas_used)`. | The fee stays execution-dependent for any `p < 1`; capacity exhaustion is only `1/p` times more expensive, not priced. Rejected for the goal; could still inform a transition if the data scan shows a large shock, but at the cost of the determinism above. |
| Keep deposits metered | Exempt deposits from the `gasUsed` change. | Header `gasUsed` would depend on executing deposits (including the L1 attributes transaction), reintroducing the execution dependency. Deposits pay no L2 fee either way. |

## Proof of concept

Status: implemented in `base-common-evm` behind `BaseSpecId::charges_gas_limit()` (Zenith), with the Tier 1 and Tier 2 tests below. System tests, the pipelining property tests, and the mainnet data scan are not yet implemented.

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

### Change 1: standard and deposit transactions

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

    // Gas limit pricing: the whole declared budget is billable and reported,
    // for deposits too, so the header gas_used is a function of the block
    // body. Simulation (`disable_fee_charge`) keeps metered gas so
    // `eth_estimateGas` still finds the gas a call needs.
    let ctx = evm.ctx();
    let cfg = ctx.cfg();
    if cfg.spec().charges_gas_limit() && !cfg.is_fee_charge_disabled() {
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
- `reward_beneficiary`: tip, base fee, and operator fee are computed from `gas().used() == gas_limit`. Deposits return early as today.
- Deposits: the deposit effective gas price is zero, so the pre-charge, reimbursement, and reward are all zero regardless of `gas_used`; only the reported gas changes. Failed deposits already report `gas_limit` (`catch_error`).
- `ResultGas::tx_gas_used = max(gas_limit - 0, floor) = gas_limit`; receipts and header follow. The builder's post-execution `cumulative_gas_used` now increases by exactly what admission reserved.
- `Gas::spend_all` does not touch the EIP-8037 reservoir. EIP-8037 is disabled on every Base upgrade (`test_eip8037_state_gas_disabled_on_all_base_upgrades`); if Base ever maps to Amsterdam, state gas needs its own decision.

### Change 2: EIP-8130 transactions

`Eip8130Executor::billable_gas` is the single source of the settled amount. Add the spec and return the reserved budget:

```rust
// crates/common/evm/src/eip8130.rs
fn billable_gas(outcome: &Eip8130Outcome, calls: &CallsResult, spec: BaseSpecId) -> u64 {
    if spec.charges_gas_limit() {
        // The amount the builder reserves: the sender budget plus payer
        // authentication metered on top of it.
        return outcome.gas_limit.saturating_add(outcome.payer_auth);
    }
    // existing net charge
    ...
}
```

`settle_fees` already derives the fee, base-fee amount, tip, and operator fee from `billable_gas`, refunds `prepay - total_cost` to the payer, and returns `billable_gas` as the receipt gas. The pre-charge bounds `gas_limit + payer_auth` at the effective price, so the subtraction cannot underflow. `simulate` stays on the gross metered amount so EIP-8130 estimation keeps working.

### What does not change

- Transaction validity, intrinsic gas, nonce and balance checks, and the EIP-7825 cap: the balance check already requires `gas_limit * max_fee + value + L1 fee + operator fee`.
- Transaction pool admission and ordering ([`crates/execution/txpool`](../../crates/execution/txpool)): ordering by effective tip per gas remains revenue-optimal because revenue is now exactly `tip * reserved_gas`.
- Builder admission ([`crates/builder/core/src/execution.rs`](../../crates/builder/core/src/execution.rs)): it already reserves `gas_limit (+ payer_auth)`; `gas_headroom` metrics become exact. One builder change is needed: the operator's per-transaction gas cap (`--builder.max_gas_per_txn`) is checked against `tx_gas_used()` after execution, which under the rule equals the declared limit. The PoC moves that check in front of execution when `charges_gas_limit()` is active (`ResourceLimits::tx_gas_limit`), so a transaction whose declared limit exceeds the cap is rejected without being executed, with the same permanent `MaxGasUsageExceeded`. The cap thereby becomes a cap on declared limits: operators that set it should expect wallet-padded transactions above it to be excluded, which is the intended meaning of a per-transaction gas cap under reservation pricing.
- Opcode gas schedule, `GAS` opcode, call stipends, and the 63/64 rule: the EVM meters gas internally so execution is identical.
- L1 data fee, DA footprint (Jovian), and batch submission.
- The L1 attributes transaction and deposit fee semantics.
- Derivation: the consensus layer does not compute gas.

## Effects

**Users.** The execution component of the fee rises from `gas_used * price` to `gas_limit * price`. A transaction that fails out-of-gas already pays its full limit today, so the worst case is unchanged; the change removes the success-path discount. Wallet padding becomes a direct cost, so wallets should move toward tight `eth_estimateGas` results. Trimming too far turns into out-of-gas failures that still pay the full limit; this tradeoff is measured in the validation plan.

**Receipts and explorers.** `gasUsed == gasLimit` for every transaction. `effectiveGasPrice * gasUsed + l1Fee (+ operatorFee)` still equals the amount debited. Explorers that display "gas used / gas limit" utilisation will show 100%; they need traces or `eth_estimateGas` for metered gas.

**Base fee.** Header `gasUsed` becomes reserved gas, so a block that is full by reservation reads as full to EIP-1559 and the base fee rises. Target and elasticity are unchanged in the POC; the base fee will trend higher after activation until limits tighten. The data scan below quantifies the step.

**Sequencer and vaults.** Tip revenue and `BaseFeeVault` inflows increase by the aggregate `gas_limit - gas_used`. The operator fee is computed on `gas_limit` and no longer refunded.

**RPC.**

| Endpoint | Behavior |
|---|---|
| `eth_estimateGas` | Unchanged: runs with `disable_fee_charge`, so the hook does not fire and the search sees metered gas. The result is now also exactly what the user is billed for if they use it as the limit. |
| `eth_call` | Unchanged for the same reason. |
| `eth_simulateV1` | Observes the rule in both `validation` modes, because the pinned reth never sets `disable_fee_charge` for it (it only disables EIP-3607, the nonce check, and the base-fee check): each simulated call's `gasUsed` is its gas limit, matching a real block. A call that omits `gas` is given the block's remaining gas and is therefore reported as consuming all of it, so later calls in the same simulated block default to a limit of 0, and the full-limit `gasUsed` is also subtracted from the request-wide `--rpc.gascap` budget (`GasLimitReached`). Base owns its `simulate_v1` ([`crates/execution/rpc/src/eth/call.rs`](../../crates/execution/rpc/src/eth/call.rs)); whether it should disable fee charging when `validation` is false is an open follow-up, not changed by the POC, and needs a multi-call `eth_simulateV1` test under Zenith either way. |
| `eth_getTransactionReceipt`, `eth_getBlockByNumber` | `gasUsed`, `cumulativeGasUsed`, header `gasUsed` report billable gas. |
| `debug_traceTransaction`, `trace_*` | Replay with fee charging enabled; top-level `gasUsed` equals `gas_limit`. Per-opcode `gas`/`gasCost` are metered and unchanged. |
| `eth_feeHistory` `gasUsedRatio` | Reflects reservation, which is the intended signal. |

**Proving.** The prover executes the same `base-common-evm` handler, so proofs follow once the fork is active in the rollup config used by the proof program. No separate change.

## Validation plan

The POC is validated at the four tiers in [TESTING.md](TESTING.md), plus a mainnet data scan. Every test runs twice where it matters: once with Zenith active and once with the latest scheduled upgrade (`Everest`), to prove the pre-activation path is untouched.

### Success criteria

The POC works when all of the following hold on a devnet with Zenith active, and none of them changes behavior on a devnet without Zenith:

| # | Criterion | Checked by |
|---|---|---|
| S1 | For every non-deposit transaction, `sender_balance_before - sender_balance_after == value + gasLimit * effectiveGasPrice + l1Fee + operatorFee(gasLimit)`, independent of how much gas execution used. | Unit (handler), system test |
| S2 | Receipt `gasUsed == gasLimit` for every transaction, deposits included; `cumulativeGasUsed` is the running sum; header `gasUsed` equals the last receipt's `cumulativeGasUsed`. | Block executor test, system test |
| S3 | Header `gasUsed` and the next block's base fee are reproduced exactly from the block body and parent header alone, without executing the block. | Block executor test (pre-execution derivation), system test |
| S4 | Fee recipients receive gas-limit-based amounts: sequencer `tip * gasLimit`, `BaseFeeVault` `basefee * gasLimit`, `OperatorFeeVault` the operator fee on `gasLimit`, `L1FeeVault` unchanged. Sum of credits equals the sender debit minus `value`. | Unit (handler), system test |
| S5 | EIP-3529 refunds and the EIP-7623 floor have no effect on the fee or on `gasUsed`. | Unit (handler) |
| S6 | `eth_estimateGas` returns the metered requirement (about 21,000 for a plain transfer), not the cap; `eth_call` returns metered gas. | System test |
| S7 | Builder and validator agree: every block the builder produces is accepted by the validator, and a payload whose header `gasUsed` is the metered sum is rejected as invalid after activation. | System tests (`fuzz_sync_parity`-style and `invalid_new_payload`-style) |
| S8 | EIP-8130: settled charge is `gasLimit + payer_auth` at the effective price; `simulate` still returns the gross metered estimate. | Unit (eip8130), system test (`eip8130`) |
| S9 | Deposits pay nothing and the L1 attributes transaction executes unchanged. | Unit (handler), block executor test |
| S10 | A proof of a Zenith block containing gas-limit-priced transactions verifies. | System test (`in_process_zk`-style) |
| S11 | Out-of-gas behavior, revert behavior, and all per-opcode gas in traces are identical with and without Zenith. | Unit (handler), trace comparison in the system test |
| S12 | Without Zenith, the full workspace test suite passes with no assertion changes. | `just test`, `just devnet tests` |

### Tier 1: unit tests

In `handler.rs` (`#[cfg(test)] mod tests`), extending the existing `call_last_frame_return` harness so it also runs `reimburse_caller` and `reward_beneficiary` against an `InMemoryDB` with funded caller, beneficiary, and vault accounts:

- Zenith, `Stop` with 90 of 100 gas remaining: `gas.remaining() == 0`, `gas.refunded() == 0`, `tx_gas_used == 100`; caller debited `100 * effective_gas_price`; beneficiary credited `100 * tip`; `BaseFeeVault` credited `100 * basefee`; `OperatorFeeVault` credited the operator fee on 100 (S1, S4).
- Zenith with `record_refund(20)`: refund is zero and the fee is unchanged (S5).
- Zenith, `OutOfGas` and `Revert`: same debit as `Stop`; `gasUsed == 100` (S11).
- Zenith, `disable_fee_charge`: `remaining == 90`, nothing debited (S6).
- Zenith deposit (`source_hash` set), `Stop` with gas remaining: `tx_gas_used == gas_limit`; depositor balance changes only by `mint - value`; beneficiary and vaults unchanged (S9).
- Everest: `test_consume_gas`, `test_revert_gas`, `test_consume_gas_with_refund`, and the rest of the existing module pass unchanged (S12).
- EIP-8037 guard: `test_eip8037_state_gas_disabled_on_all_base_upgrades` still passes, so `spend_all` leaving the reservoir alone is safe.

In `eip8130.rs`: `billable_gas == gas_limit + payer_auth` under Zenith regardless of `call_gas_spent` and refunds; payer refund `== prepay - total_cost`; `simulate` returns the gross metered estimate; Everest unchanged (S8).

In `spec.rs`: `charges_gas_limit()` is true only for Zenith and false for every variant in `BaseUpgrade::EXECUTION_VARIANTS`.

### Tier 2: block executor test

In `block_executor.rs`, execute a block under Zenith containing the L1 attributes deposit, a plain transfer with `gas_limit = 1_000_000`, and a contract call that reverts:

- Header `gasUsed == 1_000_000 + call_gas_limit + attributes_gas_limit` and equals the last receipt's `cumulativeGasUsed` (S2).
- Compute the expected header `gasUsed` by summing gas limits from the block body, and the next base fee with `compute_jovian_base_fee` from a header carrying that sum and the body-derived `blobGasUsed`; both must equal the values from the executed block (S3). This is the test that proves the pipelining property.
- Vault balance deltas match S4; the depositor's balance is unchanged by settlement (S9).
- The same block under Everest produces metered `gasUsed` and the existing vault behavior (S12).

### Tier 3: system tests

In `etc/systems/tests`, using `SystemTestStackBuilder::with_base_zenith_activation_block` as `builder_cutover.rs` does, with a sibling run without Zenith:

1. **Fee and receipt invariants.** Fund an account, send a plain transfer with `gas_limit = 1_000_000` and a non-zero tip. Assert S1 from `eth_getBalance` before and after (using `l1Fee` and `operatorFee` from the Base receipt), S2 from the receipt and `eth_getBlockByNumber`, and S6 from `eth_estimateGas` on the same call (expect `21_000`, allow the EIP-7623 floor). Compare `debug_traceTransaction` per-opcode gas with the pre-Zenith run (S11).
2. **Pre-execution derivation.** For every block in a window after activation, fetch the body and parent header, compute `gasUsed` and the next base fee locally, and assert they equal the header values and the child's `baseFeePerGas` (S3).
3. **Builder and validator agreement.** Run the builder and client together across activation and assert the client follows every builder block (the `smoke_test_builder_and_client_block_sync` pattern); run the sync-parity fuzz profile with Zenith enabled (S7).
4. **Invalid payload rejection.** Submit a `newPayload` whose header `gasUsed` is the metered sum for a block after activation and assert the client returns `INVALID` (the `invalid_new_payload.rs` pattern) (S7).
5. **EIP-8130.** Run the existing `eip8130.rs` system test with Zenith and assert the payer debit is `(gas_limit + payer_auth) * effective_gas_price + l1Fee + operatorFee` (S8).
6. **Proving.** Run the in-process proof test on a Zenith block containing the transfer above (S10).
7. **Zenith off.** Run the same suite without Zenith; every assertion about metered gas must hold as today (S12).

### Tier 4: full suite

### Demo

[`etc/scripts/devnet/gas-limit-pricing-demo.sh`](../../etc/scripts/devnet/gas-limit-pricing-demo.sh) shows the rule on a local devnet. Start one with Zenith scheduled and run the script once the chain passes the activation block (100 by default):

```sh
just devnet up-single zenith
./etc/scripts/devnet/gas-limit-pricing-demo.sh            # waits for block 100, then runs
TX_COUNT=10 GAS_LIMIT=5000000 ./etc/scripts/devnet/gas-limit-pricing-demo.sh
```

It sends `TX_COUNT` plain transfers from a prefunded devnet account, each declaring `GAS_LIMIT` gas (default 1,000,000) for a call that needs about 21,000, then prints per-transaction receipts and checks: every receipt's `gasUsed` equals the declared limit (S2); the sender's balance delta equals `sum(value + gasUsed * effectiveGasPrice + l1Fee)` (S1); `eth_estimateGas` still returns the metered requirement (S6); and each touched block's header `gasUsed` equals the sum of its transactions' declared gas limits (S3). It also prints what the same transfers would have cost at metered gas. Running it against a devnet without Zenith (`just devnet up-single`) shows the pre-activation behavior: `gasUsed` of 21,000 and a balance delta to match.

### Testing the pipelining property

Pipelined agreement is not implemented by this POC, so it cannot be tested end to end yet. What can be tested now is the property the fee rule promises pipelining: everything a validator needs to agree on a block is a function of the block body, the parent header, and the parent state. The tests below are staged by what exists.

**Stage A: with the POC only.**

1. **Pre-execution oracle, differentially fuzzed.** Write a `predict_block(body, parent_header, parent_state) -> Prediction` that, without executing any transaction, returns header `gasUsed`, `blobGasUsed`, the next base fee, and per-transaction `(sender debit, sequencer credit, BaseFeeVault credit, OperatorFeeVault credit, L1FeeVault credit)`. Execute the same block with `BaseBlockExecutor` and assert the prediction equals the executed header and the observed balance deltas. Drive it with the `fuzz_sync_parity` transaction generator (random types, limits, calldata, reverts, out-of-gas, EIP-8130, deposits) so the property is checked over thousands of blocks, not three hand-written ones. Any divergence means a validator that agreed pre-execution would have agreed to a wrong header or fee: the pipelining property is broken. Pass: zero divergences across the fuzz budget.
2. **Admission sufficiency.** For every fuzzed block, re-run the proposer's pre-validation (nonce continuity and `balance >= value + B * max_fee + l1_fee + operator_fee(B)` against parent state plus same-sender in-block reservations) and assert that every transaction it admits executes without an execution-time validity error. Pass: zero admitted-then-invalid transactions for plain EOAs. Run the same check with an EIP-7702-delegated sender drained by an earlier transaction in the block; this test is expected to fail and is kept as the executable statement of the open question in [Risks](#risks-and-open-questions) until the pipelining design resolves it.
3. **Reservation pricing under adversarial load.** Flood a devnet with transactions that declare the EIP-7825 cap (`2^24` gas) and exit after about 25,000 gas. Pass: each pays `2^24 * effectiveGasPrice`, the block's header `gasUsed` reaches the block gas limit while metered execution stays near zero, and the base fee rises across consecutive blocks per EIP-1559. This shows capacity exhaustion costs the full block price and that the fee market reacts to reservation.
4. **Agreement commitment emulation in the action harness.** Add a pipelined-verifier actor to [`actions/harness`](../../actions/harness) that, on receiving block N+1, computes the pre-execution commitment (body hash plus `gasUsed`, `blobGasUsed`, base fee) and records it as agreed before executing block N+1; after execution it checks the executed header against the commitment. Pass over long fuzzed runs: no agreed commitment is ever contradicted by execution, and the verifier never has to roll back an agreed block. The actor also doubles as the reference implementation of what the real pipelined validator must compute.

**Stage B: once pipelined consensus exists.**

5. **Agreement latency is independent of execution time.** In the system benches ([`etc/systems/benches`](../../etc/systems/benches)), build blocks whose execution cost is varied from near-empty to the full block gas limit of compute-heavy transactions. Measure time from body availability to agreement, and time from agreement to execution complete. Pass: agreement latency is flat across the execution-cost sweep (within noise), while in the sequential configuration it grows with execution cost.
6. **Throughput gain.** Same bench, pipelined versus sequential, at a fixed block gas limit and block time. Report gas per second and blocks per second. Pass: the pipelined configuration sustains the target block time at an execution load where the sequential configuration misses it; the exact gain is the deliverable, not a threshold.
7. **Bounded execution lag.** Under sustained load, the number of agreed-but-unexecuted blocks stays at the designed depth (one, in the model described here) and never grows. Pass: lag never exceeds the configured depth over the run.
8. **Failure injection.** Agree on block N+1, then make block N fail execution (inject an invalid state root at verification time). The protocol must handle N+1 per the pipelining spec (discard or re-base). This test belongs to the pipelining design but depends on the fee rule: it must show that N+1's pre-execution commitments were correct even though N was bad, so nothing about N+1's fees has to be recomputed.

Stage A tests 1, 2, and 4 are the ones that matter for this POC: they prove the fee rule delivers what pipelining needs and pin the known gap. Stage B measures the benefit and can only run once the consensus change exists.

`just test` and `just devnet tests` with no changes to existing assertions (S12). Any existing test that needs its assertion changed is a sign the gate leaked into the pre-activation path and must be investigated, not updated.

### Mainnet data scan

Not a correctness test, but part of validating the design: scan a recent range of Base Mainnet blocks (a `base-bench` subcommand in [`etc/systems`](../../etc/systems), next to the DA block-range measurement, or a one-off script over `eth_getBlockReceipts`) and report, per transaction type and per top contract: the distribution of `gas_limit / gas_used`, the fee multiplier users would have paid, the share of blocks whose reserved gas exceeds the EIP-1559 target, and the resulting base-fee trajectory. This needs no re-execution: `gasLimit` and `gasUsed` are in the transactions and receipts. The scan sizes the user communication and informs any target change in the specification phase.

## Risks and open questions

- **In-block balance reduction by another sender (EIP-7702, EIP-8130).** The "invalid at execution time means invalid block" rule assumes a sender's balance can only be lowered in-block by that sender's own transactions, which the proposer accounts for. An EIP-7702-delegated EOA whose delegate code lets a third party move its ETH, or an EIP-8130 payer whose policy allows it, can have its balance drained by an earlier transaction in the same block, making an honestly pre-validated later transaction fail its fee check at execution. Options: (a) proposer treats senders with delegated code or payer policies conservatively (require balance net of the maximum in-block outflow, or exclude them from the same block as transactions that can touch them); (b) a consensus rule that such a transaction is included with a failed receipt, its nonce bumped and its fee taken from available balance, leaving the block valid; (c) accept the hazard for the POC and resolve it in the pipelining spec. This is the main open design question for the pipelined protocol and must be settled before the rule graduates.
- **Fee shock.** Users with 2x to 5x padded limits see 2x to 5x execution fees. Mitigations: measurement first, wallet outreach, and an announcement window. A per-transaction gas limit cap tighter than EIP-7825's `2^24` bounds the worst case.
- **Estimation becomes load-bearing.** `eth_estimateGas` results that are too low turn into out-of-gas failures that pay the full limit; results that are high are paid in full. Calls whose gas depends on state at inclusion time (swaps, auctions) cannot be estimated exactly, so some padding remains rational. The data scan should report how often the optimistic estimate would have failed.
- **Semantics of `gasUsed`.** The field changes meaning for a widely used audience. Explorer and indexer owners should review before the rule is specified, and the metered gas should remain reachable through traces.
- **Base-fee transition.** Reserved gas is higher than metered gas, so the base fee steps up after activation until limits tighten. Decide, from the data scan, whether to change the target or elasticity alongside activation or accept the transition.
- **EIP-8130 payer authentication.** Charging `payer_auth` at the full reserved amount is consistent with reservation but differs from today's floor-versus-standard-rate settlement. Confirm this matches the intended EIP-8130 fee model.
- **EIP-8130 and `disable_fee_charge`.** `Eip8130Executor::execute` settles fees regardless of `cfg.disable_fee_charge` (pre-existing), so a signed execute-mode EIP-8130 transaction run under that flag reports the reserved budget rather than metered gas. EIP-8130 estimation goes through `simulate`, which is unaffected, so normal RPC paths are fine; the exemption is by execution mode, not by the flag.
- **Deposit `gasUsed` regression to Bedrock shape.** Tooling that was updated for Regolith (deposit `gasUsed` metered) sees deposits report `gasLimit` again. Low impact, but it should be in the specification.
- **Divergence from the OP Stack fee model.** Base-specific execution rules already exist (Azul onward), but this rule needs a specification entry, and shared tooling that assumes `gasUsed < gasLimit` for successful calls must be checked.

## Rollout

1. Implement the POC behind `BaseSpecId::charges_gas_limit()` (Zenith), with the unit, block-executor, and system tests above. No change to Base Sepolia or Base Mainnet configs.
2. Run the mainnet data scan and publish the fee-multiplier and base-fee results.
3. Resolve the in-block balance reduction question with the pipelining design; review `gasUsed` semantics with explorer and wallet stakeholders.
4. Write the specification and move the gate to a named upgrade per [UPGRADES.md](UPGRADES.md). Schedule on Base Sepolia, observe estimation behavior and failure rates, then Base Mainnet.

## References

- [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559): fee market and base-fee update from header `gasUsed`.
- [EIP-3529](https://eips.ethereum.org/EIPS/eip-3529): refund cap (`gas_used / 5`).
- [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623): calldata floor.
- [EIP-7702](https://eips.ethereum.org/EIPS/eip-7702): EOA code delegation.
- [EIP-7778](https://eips.ethereum.org/EIPS/eip-7778): block gas accounting without refunds (L1 precedent for counting reserved rather than net gas).
- [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825): transaction gas limit cap.
- [EIP-8130](https://eips.ethereum.org/EIPS/eip-8130): Base transactions.
- Code: [`handler.rs`](../../crates/common/evm/src/handler.rs), [`eip8130.rs`](../../crates/common/evm/src/eip8130.rs), [`l1block.rs`](../../crates/common/evm/src/l1block.rs), [`block_executor.rs`](../../crates/common/evm/src/executor/block_executor.rs), [`basefee.rs`](../../crates/execution/chainspec/src/basefee.rs), [`builder execution.rs`](../../crates/builder/core/src/execution.rs), [`UPGRADES.md`](UPGRADES.md), [`TESTING.md`](TESTING.md).
