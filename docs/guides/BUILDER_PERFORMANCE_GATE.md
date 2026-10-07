# Builder performance gate

The builder performance gate is a per-PR check that fails when a change makes the block build path slower or noisier than its pinned budget. It covers both builders: the flashblocks builder (`crates/builder/core`) and the native payload builder (`crates/execution/payload`) that serves blocks once Denim activates. It runs in `.depot/workflows/bench-builder-gate.yml` as the `Builder performance gate` job.

The build path's cost depends on pool shape as much as on code. A backlog of validity transactions whose predicates stay unsatisfied is re-considered, re-evaluated, and re-parked on every flashblock and after every state change that wakes it, so any per-candidate cost on the builder thread (an event, an allocation, a redundant read) is multiplied by the backlog size. Benchmarks that run with the transaction event writer off, or without a resting backlog, do not see that cost. The gate runs the real loop with events on against a fixed matrix of pool shapes, and fails when a change exceeds the pinned budgets.

## What it runs

Every scenario runs once per builder. Budgets are keyed `<builder>/<scenario>`, for example `flashblocks/resting_backlog` and `native/resting_backlog`.

### Flashblocks builder

Each scenario builds one block of 10 flashblocks through the production build loop, using `FlashblockBlockDriver` in `crates/builder/core/src/flashblocks/block_driver.rs`:

1. `execute_pre_steps` for the block.
2. For each flashblock: refresh `BestFlashblocksTxs` from the pool, run `BasePayloadBuilderCtx::execute_best_transactions` with a block-lived `BlockDeferrals`, mark the included transactions committed, and run `build_block` without a state root.
3. A finalizing `build_block` with the state root, followed by `BUILDER_PAYLOAD_FINALIZED` and one `BUILDER_INCLUDED` per transaction.

This covers the per-flashblock decision loop (candidate iteration, predicate evaluation, parking, parked rescan after state changes, deferral bookkeeping), EVM execution, receipts and transactions roots, flashblock payload assembly, the MDBX state root, and transaction event construction and serialization.

### Native builder

Each scenario builds one Denim block through `base_execution_payload_builder::builder::Builder::build`, the code `BasePayloadBuilder::try_build` runs, with Denim active at genesis. A Denim build is one pass: pre-execution changes (including the `BaseTime` predeploy link, so the genesis holds its proxy), `execute_best_transactions` over the whole pool, and `finish` with the state root, returning a frozen payload. Every transaction the flashblocks run would add between flashblocks is already in the pool, and the block gas limit is the workload's total gas target (the sum of the per-flashblock targets), so both builders include the same transactions.

This covers the native decision loop (candidate iteration, predicate evaluation, parking, parked rescan after state changes, deferral bookkeeping), EVM execution, the MDBX state root, and transaction event construction and serialization. The native builder journals only validity-gated candidates, so its event budgets are smaller: plain transfers emit no builder events.

### Shared fixture

State comes from reth's MDBX test provider, seeded through genesis with every sender, so reads and the state root go through the same provider code as a node. Transaction events go to the production file writer (`TransactionEventWriter::from_config`), which serializes on the builder thread and hands lines to its writer thread.

Both builders run with a per-transaction and per-block DA limit and, for flashblocks, an uncompressed block size limit, set far above the workload (`FlashblockWorkload::MAX_DA_TX_SIZE` and its siblings). The limit checks therefore run on every candidate, as on a builder with DA throttling enabled, without rejecting any. The flashblocks driver splits the DA target evenly across flashblocks, as `build_next_flashblock` does. The DA footprint limit stays off because the synthetic chain has no Jovian L1 block info.

The pool is a bare `PendingPool` read through `ParkedBestTransactions::new(pool.best(), ...)` with `no_updates()`. Production wraps the protocol pool and the nonce-lane pool in `MergeBestTransactions` with the real base fee and accepts arrivals during a build; the gate has one lane, a zero base fee, and no arrivals mid-build. The rejection cache uses the production defaults (`REJECTION_CACHE_MAX_CAPACITY`, `REJECTION_CACHE_TTL`); no scenario produces permanent rejections today.

The workloads live in `crates/builder/core/src/test_utils/flashblock_workload.rs`. All transactions are 21,000-gas transfers from unique senders. Validity transactions out-tip transfers, so every flashblock, and the native pass, reaches the whole backlog before the transfers.

| Scenario | Pool traffic per block | What it isolates |
| --- | --- | --- |
| `transfers` | 100 transfers arrive before each flashblock | Baseline cost of execution, payload assembly, state root, and accepted/included events |
| `resting_backlog` | Baseline plus 4,500 single-predicate validity transactions that never become satisfied | 4,500 candidates re-considered, re-evaluated, and re-parked on every flashblock (45,000 deferrals; 4,500 on the native pass) |
| `resting_backlog_multi_predicate` | Backlog with 8 predicates per transaction on unique accounts | Predicate evaluation cost and cold reads per candidate |
| `resting_backlog_shared_state` | Backlog with 8 predicates per transaction over 16 shared accounts | Warm reads and crowded predicate-index buckets |
| `wake_rescan` | 1,000 parked transactions on 125 shared accounts (8 per flat bucket); every transfer pays one of them | Bucket wakeups, parked rescan, and re-park after each commit |
| `backlog_growth` | 1,500 resting at start, 300 more arrive before each flashblock | Pool churn and backlog growth within a block (native: 4,500 at start) |
| `congested` | 300 transfers arrive per flashblock, 100 fit | Gas-limit rejections of a growing overflow (native: 3,000 transfers, 1,000 fit) |
| `satisfied_validity` | Baseline plus 50 validity transactions per flashblock whose 4 predicates hold | Predicate evaluation on the inclusion path |

The event-type mix is pinned per scenario rather than derived from production volume, so the budgets do not drift when production traffic changes.

## Measurement

The job measures two things, and both fail closed.

**Event volume.** `crates/builder/core/tests/flashblock_build_gate.rs` runs every scenario on both builders with events captured and fails when a scenario emits more events of a type than its budget in `etc/benchmarks/builder-gate-budgets.json` allows. Event types without a budget must not be emitted. Counts are exact, so these budgets have no headroom.

**Instruction counts.** `crates/builder/core/benches/flashblock_build_iai.rs` runs each scenario on both builders (`build_block` and `build_native_block`) under Valgrind Callgrind through iai-callgrind 0.16.1. Fixture construction runs in the unmeasured setup phase. Callgrind counts only the thread running the benchmark function, which is the builder thread; the event writer's background file I/O is not counted. `etc/scripts/ci/builder_gate_check.py` then checks two budgets per scenario:

- `max_instructions`: the pinned baseline plus `headroom_pct` (5%).
- `max_marginal_instructions_per_deferral`, for backlog scenarios: `(instructions(scenario) - instructions(transfers)) / deferrals_per_block`, against the same builder's `transfers`, plus `marginal_headroom_pct` (5%). It is the cost of one deferred candidate, the unit that a resting backlog multiplies. Because it subtracts the baseline from the same run, it is less sensitive to changes that shift every scenario.

Instruction counts are deterministic for a fixed toolchain, dependency set, target, and Valgrind version, so they work on shared runners. Wall-clock measurement on shared runners is too noisy for a gate, which is why `bench-pr.yml` is manual only. Wall-clock control flow inside the loop would make counts nondeterministic under Valgrind, so the fixture disables the predicate evaluation cutoff (`predicate_eval_hard_cutoff = Duration::MAX`) and sizes the event writer's lossy queue so it never drops.

## Baselines and budgets

Pinned on `x86_64-unknown-linux-gnu`, `depot-ubuntu-24.04-16`, Rust 1.96.0, Valgrind 3.22.0, at commit `b92ac3e0c` (Depot run `mvzp46ckqs`).

Flashblocks builder (`deferrals_per_block`: 45,000 for `resting_*`, 10,000 for `wake_rescan`, 31,500 for `backlog_growth`):

| Scenario | Baseline instructions | Budget | Marginal per deferral (budget) |
| --- | ---: | ---: | ---: |
| `transfers` | 517,649,734 | 543,532,221 |  |
| `resting_backlog` | 1,601,724,165 | 1,681,810,374 | 24,091 (25,296) |
| `resting_backlog_multi_predicate` | 2,029,540,745 | 2,131,017,783 | 33,598 (35,278) |
| `resting_backlog_shared_state` | 1,724,418,604 | 1,810,639,535 | 26,817 (28,158) |
| `wake_rescan` | 776,858,233 | 815,701,145 | 25,921 (27,217) |
| `backlog_growth` | 1,398,178,256 | 1,468,087,169 | 27,953 (29,351) |
| `congested` | 1,935,233,484 | 2,031,995,159 |  |
| `satisfied_validity` | 806,559,832 | 846,887,824 |  |

Native builder (`deferrals_per_block`: 4,500 for `resting_*` and `backlog_growth`, 1,000 for `wake_rescan`):

| Scenario | Baseline instructions | Budget | Marginal per deferral (budget) |
| --- | ---: | ---: | ---: |
| `transfers` | 147,652,939 | 155,035,586 |  |
| `resting_backlog` | 519,493,086 | 545,467,741 | 82,631 (86,763) |
| `resting_backlog_multi_predicate` | 798,048,966 | 837,951,415 | 144,532 (151,760) |
| `resting_backlog_shared_state` | 498,843,099 | 523,785,254 | 78,042 (81,945) |
| `wake_rescan` | 244,571,597 | 256,800,177 | 96,919 (101,765) |
| `backlog_growth` | 519,144,389 | 545,101,609 | 82,554 (86,682) |
| `congested` | 290,670,659 | 305,204,192 |  |
| `satisfied_validity` | 258,248,797 | 271,161,237 |  |

A native deferral costs more than a flashblocks deferral (about 83,000 against 24,000 instructions) because every native deferral is a first evaluation with a `BUILDER_DEFERRED` event, while most flashblocks deferrals are cheap re-parks of a transaction already deferred in the block. `native/wake_rescan` also pays for about 8,000 rescans spread over its 1,000 deferrals.

Flashblocks event budgets per block:

| Scenario | `BUILDER_ACCEPTED` | `BUILDER_INCLUDED` | `BUILDER_DEFERRED` | `BUILDER_REJECTED` | `BUILDER_PAYLOAD_FINALIZED` |
| --- | ---: | ---: | ---: | ---: | ---: |
| `transfers` | 1,000 | 1,000 | 0 | 0 | 1 |
| `resting_backlog`, `_multi_predicate`, `_shared_state` | 1,000 | 1,000 | 4,500 | 0 | 1 |
| `wake_rescan` | 1,000 | 1,000 | 1,000 | 0 | 1 |
| `backlog_growth` | 1,000 | 1,000 | 4,500 | 0 | 1 |
| `congested` | 1,000 | 1,000 | 0 | 11,000 | 1 |
| `satisfied_validity` | 1,500 | 1,500 | 0 | 0 | 1 |

Native event budgets per block: `BUILDER_DEFERRED` is 4,500 for `resting_*` and `backlog_growth` and 1,000 for `wake_rescan`, `BUILDER_ACCEPTED` is 500 for `satisfied_validity`, and every other type is 0. The native builder emits no events for plain transfers, including gas-limit rejections, and no `BUILDER_INCLUDED` or `BUILDER_PAYLOAD_FINALIZED` from `Builder::build`.

`BUILDER_DEFERRED` is budgeted at one per resting transaction and reason per block on both builders, matching `BlockDeferrals`. `congested` pins today's behavior: gas-limit rejections are not deduplicated, so the overflow is re-rejected on every flashblock (200 + 400 + ... + 2,000 = 11,000 events for 1,000 inclusions). That is the same per-flashblock amplification a resting backlog has; the budget stops it from growing without endorsing it.

Why 5%: counts are not bit-identical run to run, because hash maps use random seeds, but across five runs of the same code every scenario stayed within 0.15% of its baseline. Headroom also has to absorb unrelated churn such as dependency bumps on `main`. At 5%, the marginal budgets fail `flashblocks/resting_backlog` on about 1,200 added instructions per deferred candidate (about 54 million per block) and `flashblocks/wake_rescan` on about 1,600 per rescanned candidate (about 8,000 rescans per block). On the native builder, `native/resting_backlog` fails on about 4,100 per deferred candidate (about 19 million per block) and `native/wake_rescan` on about 600 per rescanned candidate. The marginal budget is the tighter of the two for every backlog scenario: 5% of the backlog's own cost is less than 5% of the whole scenario, so it catches a per-deferral regression that the absolute budget would absorb, while the absolute budget catches regressions in the shared baseline. Marginal costs varied by less than 0.3% across runs of one commit.

## When the gate fails

1. Read the job summary. It lists each scenario's count, budget, change from baseline, and marginal cost.
2. If the regression is unintended, fix it. To reproduce the counts locally on Linux with Valgrind and `iai-callgrind-runner@0.16.1`, run `cargo bench -p base-builder-core --bench flashblock_build_iai`. On macOS, run `cargo test --profile bench -p base-builder-core --test flashblock_build_gate -- --nocapture` for event counts and wall-clock build times of both builders.
3. If the cost is intended, or a toolchain or dependency bump moved every count, re-pin in the same PR. Dispatch the workflow on the branch with `pin: true` (`depot ci dispatch --repo base/base --workflow bench-builder-gate.yml --ref <branch> --input pin=true`), copy the re-pinned JSON from the job summary (or the `Re-pin budgets` step log) into `etc/benchmarks/builder-gate-budgets.json`, update the tables in this doc, and explain the change in the PR description. `--pin` records the target, runner, toolchain, Valgrind version, commit, and run in `measured_on`, and refuses to pin when a measured scenario has no entry in the budget file. A new scenario needs a budget entry (with its event budgets and, for backlog scenarios, `marginal_reference` and `deferrals_per_block`) before it can be pinned. Reviewers own whether the new budget is acceptable.
4. When a change makes the build path cheaper, re-pin so the improvement becomes the new budget.

## CI wiring

The workflow runs on `pull_request`, `merge_group`, and `workflow_dispatch`. On PRs, the measurement steps run only when `etc/scripts/local/affected-crates.py` reports `base-builder-core` as affected (a change to `base-execution-payload-builder` or another dependency affects it too), or a gate input changes (`Cargo.toml`, `Cargo.lock`, `.cargo/`, `rust-toolchain.toml`, the budget file, the checker scripts, the setup action, or the workflow). Otherwise the job reports success with a skip note, so it can be a required check without blocking unrelated PRs. Merge-queue and manual runs always measure. The path filter reads the PR's changed files through the REST API, so the workflow grants `pull-requests: read`.

The checker fails closed on its inputs as well as its budgets: a benchmark id that matches no builder (for example after renaming a benchmark function), a scenario measured without a budget, a budget without a measurement, and a `marginal_reference` from another builder are all errors.

Cost per affected PR is one `depot-ubuntu-24.04-16` runner for about 8 to 9 minutes, covering both builders, most of it the bench-profile build. Memory peaks at about 49% on this runner; the 8-vCPU runner peaked above 90% while building the bench profile, close enough to its limit that an out-of-memory kill would read as a gate failure. The event-volume test shares the bench-profile artifacts.

Making the check required is a branch-protection decision for the repository owners. Add `Builder performance gate` to the required checks for `main` and the merge queue. Until then it runs on every affected PR and fails visibly, but does not block merges.

## Not covered

- The async payload job around the loop: websocket publication, `update_accounts`, `prune_transactions`, invalidation and expiry sweeps, metering-provider bookkeeping, and the per-flashblock `BUILDER_FLASHBLOCK_*` lifecycle events. These need a live node and do not scale with the backlog.
- The predicate evaluation cutoff (`predicate_eval_hard_cutoff`) and per-transaction execution-time limits, because they are wall-clock based.
- Lock contention, allocator behavior under concurrency, and I/O latency. Callgrind counts instructions, not time. Changes that only move work to another thread, such as a writer-thread deferral, show up as a builder-thread decrease, which is the latency-relevant direction.
- Isthmus-and-later header work (withdrawals root) and blob fields; the synthetic chain activates only L1 forks through Cancun.
- For the native builder: the `BasicPayloadJob` around `try_build` (interval scheduling, finalization requests, cached reads, and the parallel state-root task; the gate computes the state root synchronously), transactions that arrive during the pass, and the pre-Denim mode in which both builders build every payload and the native builder rebuilds on each interval.
- Backlog growth across blocks. The gate pins per-block cost at a fixed backlog size; long-term trends in production build time need production monitoring.
