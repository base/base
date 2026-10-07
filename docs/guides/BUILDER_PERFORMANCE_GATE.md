# Builder performance gate

The builder performance gate is a per-PR check that fails when a change makes the flashblock build path slower or noisier than its pinned budget. It runs in `.depot/workflows/bench-builder-gate.yml` as the `Builder performance gate` job.

The build path's cost depends on pool shape as much as on code. A backlog of validity transactions whose predicates stay unsatisfied is re-considered, re-evaluated, and re-parked on every flashblock, so any per-candidate cost on the builder thread (an event, an allocation, a redundant read) is multiplied by the backlog size and the flashblock count. Benchmarks that run with the transaction event writer off, or without a resting backlog, do not see that cost. The gate runs the real loop with events on against a fixed matrix of pool shapes, and fails when a change exceeds the pinned budgets.

## What it runs

Each scenario builds one block of 10 flashblocks through the production build loop, using `FlashblockBlockDriver` in `crates/builder/core/src/flashblocks/block_driver.rs`:

1. `execute_pre_steps` for the block.
2. For each flashblock: refresh `BestFlashblocksTxs` from the pool, run `BasePayloadBuilderCtx::execute_best_transactions` with a block-lived `BlockDeferrals`, mark the included transactions committed, and run `build_block` without a state root.
3. A finalizing `build_block` with the state root, followed by `BUILDER_PAYLOAD_FINALIZED` and one `BUILDER_INCLUDED` per transaction.

This covers the per-flashblock decision loop (candidate iteration, predicate evaluation, parking, parked rescan after state changes, deferral bookkeeping), EVM execution, receipts and transactions roots, flashblock payload assembly, the MDBX state root, and transaction event construction and serialization.

State comes from reth's MDBX test provider, seeded through genesis with every sender, so reads and the state root go through the same provider code as a node. Transaction events go to the production file writer (`TransactionEventWriter::from_config`), which serializes on the builder thread and hands lines to its writer thread.

The workloads live in `crates/builder/core/src/test_utils/flashblock_workload.rs`. All transactions are 21,000-gas transfers from unique senders. Validity transactions out-tip transfers, so every flashblock reaches the whole backlog.

| Scenario | Pool traffic per block | What it isolates |
| --- | --- | --- |
| `transfers` | 100 transfers arrive before each flashblock | Baseline cost of execution, payload assembly, state root, and accepted/included events |
| `resting_backlog` | Baseline plus 4,500 single-predicate validity transactions that never become satisfied | 4,500 candidates re-considered, re-evaluated, and re-parked on every flashblock (45,000 deferrals) |
| `resting_backlog_multi_predicate` | Backlog with 8 predicates per transaction on unique accounts | Predicate evaluation cost and cold reads per candidate |
| `resting_backlog_shared_state` | Backlog with 8 predicates per transaction over 16 shared accounts | Warm reads and crowded predicate-index buckets |
| `wake_rescan` | 1,000 parked transactions on 125 shared accounts (8 per flat bucket); every transfer pays one of them | Bucket wakeups, parked rescan, and re-park after each commit |
| `backlog_growth` | 1,500 resting at start, 300 more arrive before each flashblock | Pool churn and backlog growth within a block |
| `congested` | 300 transfers arrive per flashblock, 100 fit | Gas-limit rejections of a growing overflow |
| `satisfied_validity` | Baseline plus 50 validity transactions per flashblock whose 4 predicates hold | Predicate evaluation on the inclusion path |

The event-type mix is pinned per scenario rather than derived from production volume, so the budgets do not drift when production traffic changes.

## Measurement

The job measures two things, and both fail closed.

**Event volume.** `crates/builder/core/tests/flashblock_build_gate.rs` runs every scenario with events captured and fails when a scenario emits more events of a type than its budget in `etc/benchmarks/builder-gate-budgets.json` allows. Event types without a budget must not be emitted. Counts are exact, so these budgets have no headroom.

**Instruction counts.** `crates/builder/core/benches/flashblock_build_iai.rs` runs each scenario under Valgrind Callgrind through iai-callgrind 0.16.1. Fixture construction runs in the unmeasured setup phase. Callgrind counts only the thread running the benchmark function, which is the builder thread; the event writer's background file I/O is not counted. `etc/scripts/ci/builder_gate_check.py` then checks two budgets per scenario:

- `max_instructions`: the pinned baseline plus `headroom_pct` (5%).
- `max_marginal_instructions_per_deferral`, for backlog scenarios: `(instructions(scenario) - instructions(transfers)) / deferrals_per_block`, plus `marginal_headroom_pct` (10%). It is the cost of one deferred candidate, the unit that a resting backlog multiplies. Because it subtracts the baseline from the same run, it is less sensitive to changes that shift every scenario.

Instruction counts are deterministic for a fixed toolchain, dependency set, target, and Valgrind version, so they work on shared runners. Wall-clock measurement on shared runners is too noisy for a gate, which is why `bench-pr.yml` is manual only. Wall-clock control flow inside the loop would make counts nondeterministic under Valgrind, so the fixture disables the predicate evaluation cutoff (`predicate_eval_hard_cutoff = Duration::MAX`) and sizes the event writer's lossy queue so it never drops.

## Baselines and budgets

Pinned on `x86_64-unknown-linux-gnu`, `depot-ubuntu-24.04-8`, Rust 1.96.0, Valgrind 3.22.0, at commit `957d465b7` (Depot run `qkb4853fp4`).

| Scenario | Baseline instructions | Budget | Marginal per deferral (budget) |
| --- | ---: | ---: | ---: |
| `transfers` | 516,697,858 | 542,532,751 | |
| `resting_backlog` | 1,599,429,633 | 1,679,401,115 | 24,061 (26,467) |
| `resting_backlog_multi_predicate` | 2,028,606,708 | 2,130,037,044 | 33,598 (36,958) |
| `resting_backlog_shared_state` | 1,720,742,163 | 1,806,779,272 | 26,757 (29,433) |
| `wake_rescan` | 754,994,314 | 792,744,030 | 23,830 (26,213) |
| `backlog_growth` | 1,395,587,851 | 1,465,367,244 | 27,901 (30,692) |
| `congested` | 1,930,821,324 | 2,027,362,391 | |
| `satisfied_validity` | 805,038,485 | 845,290,410 | |

Event budgets per block:

| Scenario | `BUILDER_ACCEPTED` | `BUILDER_INCLUDED` | `BUILDER_DEFERRED` | `BUILDER_REJECTED` | `BUILDER_PAYLOAD_FINALIZED` |
| --- | ---: | ---: | ---: | ---: | ---: |
| `transfers` | 1,000 | 1,000 | 0 | 0 | 1 |
| `resting_backlog`, `_multi_predicate`, `_shared_state` | 1,000 | 1,000 | 4,500 | 0 | 1 |
| `wake_rescan` | 1,000 | 1,000 | 1,000 | 0 | 1 |
| `backlog_growth` | 1,000 | 1,000 | 4,500 | 0 | 1 |
| `congested` | 1,000 | 1,000 | 0 | 11,000 | 1 |
| `satisfied_validity` | 1,500 | 1,500 | 0 | 0 | 1 |

`BUILDER_DEFERRED` is budgeted at one per resting transaction and reason per block, matching `BlockDeferrals`. `congested` pins today's behavior: gas-limit rejections are not deduplicated, so the overflow is re-rejected on every flashblock (200 + 400 + ... + 2,000 = 11,000 events for 1,000 inclusions). That is the same per-flashblock amplification a resting backlog has; the budget stops it from growing without endorsing it.

Why 5%: counts are reproducible run to run on the same runner image, so headroom only has to absorb unrelated churn such as dependency bumps on `main` (a few percent at most). At 5%, `resting_backlog` fails on about 80 million added instructions per block, or about 1,800 instructions per deferred candidate. Today each deferral costs about 24,000 instructions above the baseline, so a 10% per-deferral regression also fails the marginal budget.

## When the gate fails

1. Read the job summary. It lists each scenario's count, budget, change from baseline, and marginal cost.
2. If the regression is unintended, fix it. To reproduce the counts locally on Linux with Valgrind and `iai-callgrind-runner@0.16.1`, run `cargo bench -p base-builder-core --bench flashblock_build_iai`. On macOS, run `cargo test --profile bench -p base-builder-core --test flashblock_build_gate -- --nocapture` for event counts and native build times.
3. If the cost is intended, or a toolchain or dependency bump moved every count, re-pin in the same PR. Dispatch the workflow on the branch with `pin: true` (`depot ci dispatch --repo base/base --workflow bench-builder-gate.yml --ref <branch> --input pin=true`), copy the re-pinned JSON from the job summary into `etc/benchmarks/builder-gate-budgets.json`, update `measured_on` and the tables in this doc, and explain the change in the PR description. Reviewers own whether the new budget is acceptable.
4. When a change makes the build path cheaper, re-pin so the improvement becomes the new budget.

## CI wiring

The workflow runs on `pull_request`, `merge_group`, and `workflow_dispatch`. On PRs, the measurement steps run only when `etc/scripts/local/affected-crates.py` reports `base-builder-core` as affected, or a gate input changes (`Cargo.toml`, `Cargo.lock`, `.cargo/`, `rust-toolchain.toml`, the budget file, the checker scripts, the setup action, or the workflow). Otherwise the job reports success with a skip note, so it can be a required check without blocking unrelated PRs. Merge-queue and manual runs always measure.

Cost per affected PR is one `depot-ubuntu-24.04-8` runner for about 11 minutes with a warm sccache, of which about 7 minutes is the bench-profile build and 2 to 3 minutes is Callgrind. The event-volume test shares the bench-profile artifacts.

Making the check required is a branch-protection decision for the repository owners. Add `Builder performance gate` to the required checks for `main` and the merge queue. Until then it runs on every affected PR and fails visibly, but does not block merges.

## Not covered

- The async payload job around the loop: websocket publication, `update_accounts`, `prune_transactions`, invalidation and expiry sweeps, metering-provider bookkeeping, and the per-flashblock `BUILDER_FLASHBLOCK_*` lifecycle events. These need a live node and do not scale with the backlog.
- The predicate evaluation cutoff (`predicate_eval_hard_cutoff`) and per-transaction execution-time limits, because they are wall-clock based.
- Lock contention, allocator behavior under concurrency, and I/O latency. Callgrind counts instructions, not time. Changes that only move work to another thread, such as a writer-thread deferral, show up as a builder-thread decrease, which is the latency-relevant direction.
- Isthmus-and-later header work (withdrawals root) and blob fields; the synthetic chain activates only L1 forks through Cancun.
- The native (non-flashblocks) payload builder in `crates/execution/payload`.
- Backlog growth across blocks. The gate pins per-block cost at a fixed backlog size; long-term trends in production build time need production monitoring.
