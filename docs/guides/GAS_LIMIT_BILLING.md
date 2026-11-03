# Gas-Limit Billing

Gas-limit billing (the `GasLimitBilling` feature, gated on the genesis-only Zenith testing upgrade) charges a Base transaction for the gas it reserves (its declared `gas_limit`) instead of the gas it consumes. It exists because the builder's fixed per-flashblock time budget was being spent on high-tip transactions that revert early and pay almost nothing. This doc covers the rule, what is implemented, what is verified, and what is still open. `BASE-` and `CHAIN-` identifiers are internal ticket references; the pending decisions are being settled in the BASE-1426 specification.

## Problem

The builder evaluates and executes candidates in priority-fee order inside a 200 ms flashblock, with a 10 ms hard budget for validity-predicate evaluation (`predicate_eval_hard_cutoff`). Ranking is by `tip` per gas, known at admission; payment is `tip * gas_used`, known after execution. An atomic-arbitrage transaction sized for a 500,000-gas execution path reverts after about 25,000 gas when the opportunity is gone, so a searcher can bid a 20x tip, sort first, consume the predicate and execution budget, and pay almost nothing. Flood the pool with these and the transactions behind them, which would have executed and paid, are deferred.

Worked example at a 0.01 gwei base fee and a 1 gwei market tip:

| | Arbitrage that executes | Reverting arb |
|---|---|---|
| `gas_limit` | 600,000 | 2,000,000 |
| `gas_used` | 500,000 | 25,000 |
| `tip` | 1 gwei | 20 gwei |
| Rank | second | first |
| Pays the sequencer, metered (`tip * gas_used`) | 0.0005 ETH | 0.0005 ETH |
| Pays the sequencer, gas-limit billing (`tip * gas_limit`) | 0.0006 ETH | 0.04 ETH |

Under gas-limit billing the reverting arb pays 80x more than today and its rank costs 67x the regular transaction's: a 20x bid costs 20x. Searchers respond by bidding a tip they will pay on the whole limit, tightening predicates so the transaction is not included unless the opportunity is there, or shrinking the limit.

What the rule does not fix: predicates that do not match are not included and pay nothing under either rule, and a tight-limit high-tip reverting transaction costs the same either way. Those are builder time-budget problems (pricing or capping predicate evaluation), out of scope here.

## Rule

For every user transaction in a block where gas-limit billing is active:

1. **Billable gas** `B = gas_limit` for standard transactions and `B = gas_limit + payer_floor` for EIP-8130 transactions: the quantity the pool ranks by and the builder reserves. `payer_floor` is the payer-authentication gas (`IntrinsicGas::max_payer_auth_cost`: the `payer_auth` signature bytes priced at the EIP-7623 floor rate), metered on top of the sender's `gas_limit`; it is zero for self-paid transactions.
2. **Settlement.** The payer pays `B * effective_gas_price` plus the L1 data fee plus the operator fee on `B`. Nothing is reimbursed; EIP-3529 refunds and the EIP-7623 floor have no effect on the fee. The sequencer receives `B * priority_fee_per_gas`, the `BaseFeeVault` `B * basefee`, the `OperatorFeeVault` the operator fee on `B`. The L1 data fee is unchanged.
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

The feature was built as a stack of pull requests: [#6334](https://github.com/base/base/pull/6334) (feature flag), [#6349](https://github.com/base/base/pull/6349) (builder cap), [#6406](https://github.com/base/base/pull/6406) (standard transactions), [#6407](https://github.com/base/base/pull/6407) (EIP-8130), [#6408](https://github.com/base/base/pull/6408) (end-to-end tests). The proof of concept is [#6229](https://github.com/base/base/pull/6229).

`BaseFeature::GasLimitBilling` is a semantic feature in the hardfork feature registry ([`feature.rs`](../../crates/common/genesis/src/feature.rs)), owned by `BaseUpgrade::Zenith`, the genesis-only testing gate (never scheduled on Base Sepolia or Mainnet; devnets use `just devnet up zenith`). Call sites check `spec.is_feature_enabled(BaseFeature::GasLimitBilling)`; moving the feature to a named upgrade is one registry line. Behavior is inert unless the feature is active. The hook lives in `crates/common/evm`; `evm2` ([#6268](https://github.com/base/base/pull/6268)) has no equivalent yet and must carry it over if it becomes canonical first.

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

Everything downstream (EIP-7623 floor, `reimburse_caller`, operator-fee refund, `reward_beneficiary`, `ResultGas`) then observes `gas_used == gas_limit`, so the block executor, both payload builders, flashblocks pending state and receipts agree by construction. This single hook is deliberate: the proof of concept patched each accounting path separately, missed the flashblocks builder, and only the system test caught the builder/validator receipts-root mismatch. `eth_call` and `eth_estimateGas` run with `disable_fee_charge`, so they stay metered and estimation still converges.

**EIP-8130** ([`eip8130.rs`](../../crates/common/evm/src/eip8130.rs)). `Eip8130Executor::billable_gas` returns `FeeCheck::max_chargeable_gas(gas_limit, payer_floor)` under the feature: exactly what `prepay` pre-charged and what the builder and txpool reserve. The `simulate` estimate stays metered but stops folding `payer_floor` into the returned limit, since settlement adds it on top; a sponsored transaction sent at `gas_limit = estimate` settles `estimate + payer_floor` once.

**Builder cap** ([`execution.rs`](../../crates/builder/core/src/execution.rs)). `--builder.max_gas_per_txn` becomes a cap on declared limits: under the feature the flashblocks builder sets `ResourceLimits::max_reserved_gas_per_tx` and rejects `gas_limit + payer_floor > cap` (`TxResources::gas_limit + TxResources::payer_auth` in the builder) before execution with the permanent `MaxGasUsageExceeded`. It is also the operator's lever for bounding the worst-case fee of a padded transaction.

## Effects

Users pay `gas_limit * price` instead of `gas_used * price`; out-of-gas already paid the limit, so only the success-path discount goes away. Wallet padding becomes a visible cost. Receipts show `gasUsed == gasLimit` (`+ payer_floor` for EIP-8130), so explorers display 100% utilisation and need traces for metered gas. Header `gasUsed` becomes reserved gas, so the base fee reacts to reserved demand and blocks hold at most `block_gas_limit` of declared gas. The prover runs the same handler, so proofs follow once the feature is in the rollup config.

| RPC | Behavior |
|---|---|
| `eth_estimateGas`, `eth_call` | Unchanged (`disable_fee_charge`); the estimate is exactly what a standard transaction is billed if used as its limit. |
| `eth_simulateV1` | Observes the rule (reth never sets `disable_fee_charge` for it): a call without `gas` is reported as consuming the block's remaining gas, so later calls get a 0 limit and the `--rpc.gascap` budget drains. Decision in BASE-1434. |
| `debug_trace*`, `trace_*` | Top-level `gasUsed` is the limit; per-opcode costs stay metered. Exposing metered gas is part of BASE-1434. |
| Receipts, blocks, `eth_feeHistory` | Report reserved gas. |

## Validation

Every test runs on both sides of the fork (Zenith vs Everest, or Jovian for the block executor).

| # | Criterion | Status |
|---|---|---|
| S1 | Payer debit `== value + B * effectiveGasPrice + l1Fee + operatorFee(B)`, whatever execution used | Verified: `handler.rs` and `eip8130.rs` unit tests, system test |
| S2 | Receipt `gasUsed == B`; cumulative sum; header equals last cumulative | Verified: block executor, action test, system test |
| S3 | Header `gasUsed` = Σ reserved gas of user txs + metered deposit gas; a tx that fits by metered gas but not declared gas is refused | Verified: block executor (deposit + three 100k transfers in a 250k block), action test, system test |
| S4 | Tip, `BaseFeeVault`, `OperatorFeeVault` on `B`; credits equal debit minus value | Verified: unit tests with an independently computed operator fee (`gas * scalar * 100 + constant`); system test seeds the operator fee on the L1 `SystemConfig` and reads the block base fee |
| S5 | EIP-3529, EIP-7702 refunds and the EIP-7623 floor have no effect | Verified: unit tests show the refund pre-feature, none under it |
| S6 | `eth_estimateGas` returns metered gas (21,000 for a transfer) | Verified: unit (`disable_fee_charge`) and system test |
| S7 | Builder and validator agree; a metered-sum header is rejected after activation | Partly: hash agreement verified (action + system). Rejection **not yet tested** |
| S8 | EIP-8130 settles `gas_limit + payer_floor`; estimate excludes `payer_floor`; sponsored tx at `gas_limit = estimate` settles once | Verified: unit tests, sponsored system test |
| S9 | Deposits pay nothing and keep metered gas | Verified: unit, block executor, action test |
| S10 | Proof of a Zenith block verifies | **Not yet tested** |
| S11 | Out-of-gas, revert and per-opcode trace gas identical across the fork | Partly: settlement verified at unit level; trace parity **not yet tested** |
| S12 | Without Zenith the full suite passes unchanged | Verified: CI on every PR |
| S13 | A reverting arb is billed `tip * B`, same as a success at that limit | Verified at unit level (`REVERT` after two `PUSH1`s); same-block ordering vs a regular tx **not yet tested** |
| S14 | Over-cap reserved gas rejected before execution under Zenith, after otherwise | Verified: `is_tx_over_limits` unit tests; `crates/builder/core/tests/smoke.rs` runs the same 100k-declared / 21k-metered transfer under a 25k cap, included on the default spec and rejected, evicted and cached as permanently rejected under Zenith |
| S15 | Reverting-arb flood exhausts the predicate budget; under Zenith each included arb pays its limit | Demo script on the PoC branch only; **not a system test** |

Tests: `handler.rs`, `eip8130.rs`, `executor/block_executor.rs`, `builder/core/src/execution.rs`, `chainspec/src/spec.rs` (unit); `actions/harness/tests/zenith/gas_limit_fee_billing.rs` (real payload builder + verifier, 0.2 s); `etc/systems/tests/gas_limit_fee_billing.rs` (Docker builder + validator, Zenith at block 40, auto-discovered into the merge queue); `crates/builder/core/tests/smoke.rs` (payload builder with `max_gas_per_txn`).

Not yet verified, in priority order: invalid-payload rejection (S7); proving (S10); trace parity (S11); reverting-arb ordering and flood as system tests (S13, S15); EIP-7825-cap reservation flood with base-fee rise.

The PoC branch's devnet demo ([#6229](https://github.com/base/base/pull/6229), porting is BASE-1436) showed the reverting arb's sequencer tip rising 24x under Zenith (21,231 billed gas to its declared 500,000) to 82x a regular transaction's. BASE-1427 holds the mainnet `gas_limit / gas_used` distribution, fee multipliers and base-fee impact.

## Open decisions

In the BASE-1426 spec unless a ticket is named:

- **Named upgrade** that carries the feature; Zenith is only the test gate.
- **Base-fee target and elasticity** at activation: reserved gas is higher than metered gas, so the base fee steps up until limits tighten. Decide from the data scan.
- **EIP-3529 and EIP-7702 refunds** become economically inert: accept, or reprice at the opcode level.
- **EIP-8130 payer authentication** billed at `payer_floor` (the reserved floor-rate amount) and excluded from the estimate, so `eth_estimateGas * gasPrice` understates a sponsored total by `payer_floor * gasPrice`. Needs EIP-8130 owner sign-off.
- **RPC** (BASE-1434): `eth_simulateV1` fee charging when `validation=false`; metered gas in traces; `Eip8130Executor::execute` ignoring `disable_fee_charge`. Must land before scheduling.
- **Static EIP-8130 coinbase tips** (BASE-1432, optional): ranked on `gas_limit`, reserved as `gas_limit + payer_floor`.
- **Amsterdam** (EIP-8037/7778): parked until [#6131](https://github.com/base/base/pull/6131) lands; `spend_all` only spends regular gas.
- **Ecosystem comms**: ERC-4337 `preVerificationGas`, wallet estimate buffers, explorers at 100% utilisation. Wallet and Rundler outcomes are in the spec.

Risks to communicate: 2x–5x fee shock for padded limits (`--builder.max_gas_per_txn` bounds the worst case below `2^24`); estimation becomes load-bearing; searchers pay for inclusion-time uncertainty; `gasUsed` changes meaning; fewer transactions per block until limits tighten; divergence from the OP Stack fee model.

## Rollout

1. Land the stack behind `GasLimitBilling` (Zenith); fill the not-yet-verified list, BASE-1434 first.
2. Close the open decisions in the spec; publish the data-scan results.
3. Review `gasUsed` semantics with explorer and wallet stakeholders; communicate the cost shift to searchers and wallets.
4. Move the feature to a named upgrade per [UPGRADES.md](UPGRADES.md); Base Sepolia, then Mainnet.

References: [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559), [EIP-3529](https://eips.ethereum.org/EIPS/eip-3529), [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623), [EIP-7778](https://eips.ethereum.org/EIPS/eip-7778), [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825), [EIP-8130](https://eips.ethereum.org/EIPS/eip-8130); [`crates/common/evm/README.md`](../../crates/common/evm/README.md).
