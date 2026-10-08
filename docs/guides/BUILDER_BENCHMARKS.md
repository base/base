# Builder benchmarks

The builder benchmarks measure the block build path on every PR that can affect it and post the results as a PR comment, comparing the PR's base commit with its head. They cover both builders: the flashblocks builder (`crates/builder/core`) and the native payload builder (`crates/execution/payload`) that serves blocks once Denim activates. They run in `.depot/workflows/bench-builder.yml` as the `Benchmarks (builder · iai)` job. The check is advisory: it never fails on a regression, so reviewers read the comment.

The build path's cost depends on pool shape as much as on code. A backlog of validity transactions whose predicates stay unsatisfied is re-considered, re-evaluated, and re-parked on every flashblock and after every state change that wakes it, so any per-candidate cost on the builder thread (an event, an allocation, a redundant read) is multiplied by the backlog size. Benchmarks that run with the transaction event writer off, or without a resting backlog, do not see that cost. These benchmarks run the real loop with events on against a fixed matrix of pool shapes.

## What it runs

Every scenario in the main matrix runs once per builder, and the `_enforce` scenarios run on the flashblocks builder only. Results are keyed `<builder>/<scenario>`, for example `flashblocks/resting_backlog` and `native/resting_backlog`.

### Flashblocks builder

Each scenario builds one block of 10 flashblocks through the production build loop, using `FlashblockBlockDriver` in `crates/builder/core/src/flashblocks/block_driver.rs`:

1. The fallback block at flashblock index 0: `execute_pre_steps` and a `build_block` without pool transactions or a state root.
2. For each flashblock at indices 1 through 10, as `build_next_flashblock` numbers them: refresh `BestFlashblocksTxs` from the pool, run `BasePayloadBuilderCtx::execute_best_transactions` with a block-lived `BlockDeferrals`, mark the included transactions committed, and run `build_block` without a state root.
3. A finalizing `build_block` with the state root, followed by `BUILDER_PAYLOAD_FINALIZED` and one `BUILDER_INCLUDED` per transaction.

This covers the per-flashblock decision loop (candidate iteration, predicate evaluation, parking, parked rescan after state changes, deferral bookkeeping), EVM execution, receipts and transactions roots, flashblock payload assembly, the MDBX state root, and transaction event construction and serialization.

### Native builder

Each scenario builds one Denim block through `base_execution_payload_builder::builder::Builder::build`, the code `BasePayloadBuilder::try_build` runs, with Denim active at genesis. A Denim build is one pass: pre-execution changes (including the `BaseTime` predeploy link, so the genesis holds its proxy), `execute_best_transactions` over the whole pool, and `finish` with the state root, returning a frozen payload. Every transaction the flashblocks run would add between flashblocks is already in the pool, and the block gas limit is the workload's total gas target (the sum of the per-flashblock targets), so both builders include the same transactions.

This covers the native decision loop (candidate iteration, predicate evaluation, parking, parked rescan after state changes, deferral bookkeeping), EVM execution, the MDBX state root, and transaction event construction and serialization. The native builder journals only validity-gated candidates, so it emits far fewer events: plain transfers emit no builder events, and `Builder::build` emits no `BUILDER_INCLUDED` or `BUILDER_PAYLOAD_FINALIZED`.

### Shared fixture

State comes from reth's MDBX test provider, seeded through genesis with every sender, so reads and the state root go through the same provider code as a node. Transaction events go to the production file writer (`TransactionEventWriter::from_config`), which serializes on the builder thread and hands lines to its writer thread.

Both builders run with a per-transaction and per-block DA limit and, for flashblocks, an uncompressed block size limit, set far above the workload (`FlashblockWorkload::MAX_DA_TX_SIZE` and its siblings). The limit checks therefore run on every candidate, as on a builder with DA throttling enabled, without rejecting any. The flashblocks driver splits the DA target evenly across flashblocks, as `build_next_flashblock` does. The DA footprint limit stays off because the synthetic chain has no Jovian L1 block info.

The pool is a bare `PendingPool` read through `ParkedBestTransactions::new(pool.best(), ...)` with `no_updates()`. Production wraps the protocol pool and the nonce-lane pool in `MergeBestTransactions` with the real base fee and accepts arrivals during a build; the benchmarks have one lane, a zero base fee, and no arrivals mid-build. The rejection cache uses the production defaults (`REJECTION_CACHE_MAX_CAPACITY`, `REJECTION_CACHE_TTL`); no scenario produces permanent rejections today.

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
| `resting_backlog_enforce`, `wake_rescan_enforce`, `backlog_growth_enforce` (flashblocks only) | The same traffic as the scenario without the suffix | The resting-predicate holdback (`--builder.resting-predicates enforce`): the resting index, the iterator's resting check that skips a resting transaction until a commit changes the state its predicate reads, and the wakeups from committed state |

Scenarios without the `_enforce` suffix run with resting predicates off, the `--builder.resting-predicates` default. `shadow` mode is not measured: it runs the same resting-index bookkeeping as `enforce` without skipping resting transactions.

The event-type mix is fixed per scenario rather than derived from production volume, so results stay comparable when production traffic changes.

`congested` reflects current flashblocks behavior: gas-limit rejections are not deduplicated, so the overflow is re-rejected on every flashblock (200 + 400 + ... + 2,000 = 11,000 `BUILDER_REJECTED` events for 1,000 inclusions).

## Measurement

Each run builds the base commit and then the head commit, and measures two things on each.

**Instruction counts.** `crates/builder/core/benches/flashblock_build_iai.rs` runs each scenario on both builders (`build_block` and `build_native_block`) under Valgrind Callgrind through iai-callgrind 0.16.1. Fixture construction runs in the unmeasured setup phase. Callgrind counts only the thread running the benchmark function, which is the builder thread; the event writer's background file I/O is not counted.

**Event volume.** `crates/builder/core/tests/flashblock_build_events.rs` runs every scenario on both builders with events captured, checks that each block includes the workload's expected transactions, and prints one `BUILDER_BENCH` line per scenario with the per-type event counts and the number of deferrals. The counts are exact.

`etc/scripts/ci/builder_bench_compare.py` combines both commits' output into the comment. For each `<builder>/<scenario>` it shows:

- The instruction count on base and head, and the change.
- The cost per deferral for backlog scenarios: `(instructions(scenario) - instructions(transfers)) / deferrals`, against the same builder's `transfers` on the same commit. It is the cost of one deferred candidate, the unit a resting backlog multiplies, and it ignores changes that shift every scenario equally.
- Every event type whose count changed.

A scenario is flagged when its instruction count or per-deferral cost moves by more than 2%, or any event count changes. When nothing is flagged, the table is collapsed. Benchmarks that do not exist on the base commit are reported as new.

Instruction counts are deterministic for a fixed toolchain, dependency set, target, and Valgrind version, so base and head measured in the same job are directly comparable on shared runners. Wall-clock measurement on shared runners is too noisy for per-PR comparison, which is why `bench-pr.yml` is manual only. Wall-clock control flow inside the loop would make counts nondeterministic under Valgrind, so the fixture disables the predicate evaluation cutoff (`predicate_eval_hard_cutoff = Duration::MAX`) and sizes the event writer's lossy queue so it never drops.

### Sensitivity

Counts are not bit-identical run to run, because hash maps use random seeds, but repeated runs of one commit stay within 0.15% per scenario and 0.3% per deferral, well inside the 2% threshold. A native deferral costs several times more than a flashblocks deferral because every native deferral is a first evaluation with a `BUILDER_DEFERRED` event, while most flashblocks deferrals are cheap re-parks of a transaction already deferred in the block. At 2%, the per-deferral column flags roughly 500 added instructions per deferred candidate on `flashblocks/resting_backlog` and roughly 1,700 on `native/resting_backlog`.

## Reading the comment

1. A flagged row is a measured change, not noise. Check whether the PR is expected to change that scenario's cost or events.
2. To reproduce instruction counts locally on Linux with Valgrind and `iai-callgrind-runner@0.16.1`, run `cargo bench -p base-builder-core --bench flashblock_build_iai`. On any platform, `cargo test --profile bench -p base-builder-core --test flashblock_build_events -- --nocapture` prints the event counts and wall-clock build times of both builders.
3. To compare two arbitrary commits, dispatch the workflow on the head branch with a `base_ref` input (`depot ci run --workflow .depot/workflows/bench-builder.yml` from a checkout, or `depot ci dispatch` with `--input base_ref=<ref>`). The results go to the job summary; comments are posted only on PRs.

A new scenario needs an entry in `FlashblockWorkload::MATRIX`, a case in the event test, and a `#[bench::...]` entry on both benchmark functions. A new benchmark function needs a builder name in `BENCH_PREFIXES` in the compare script; unrecognized benchmark ids are listed in the comment.

## CI wiring

The workflow runs on `pull_request` and `workflow_dispatch`. On PRs, the measurement steps run only when `etc/scripts/local/affected-crates.py` reports `base-builder-core` as affected (a change to `base-execution-payload-builder` or another dependency affects it too), or a benchmark input changes (`Cargo.toml`, `Cargo.lock`, `.cargo/`, `rust-toolchain.toml`, the compare and comment scripts, the setup action, or the workflow). Otherwise the job succeeds with a skip note and posts no comment. Comments are skipped on PRs from forks, whose `GITHUB_TOKEN` cannot write. The comment is identified by the `<!-- builder-bench-results -->` marker and updated in place on each push.

The job fails only when the run itself fails, for example when the head commit does not build; the comment then points at the run log. A failure on the base commit is reported in the comment as missing base results.

Cost per affected PR is one `depot-ubuntu-24.04-16` runner for two bench-profile builds and two measurement passes; the base build is mostly served from the sccache cache of `main`. A single pass took 8 to 9 minutes with memory peaking at about 49%; the 8-vCPU runner peaked above 90% while building the bench profile.

## Not covered

- The async payload job around the loop: websocket publication, `update_accounts`, `prune_transactions`, invalidation and expiry sweeps, metering-provider bookkeeping, and the per-flashblock `BUILDER_FLASHBLOCK_*` lifecycle events. These need a live node and do not scale with the backlog.
- The predicate evaluation cutoff (`predicate_eval_hard_cutoff`) and per-transaction execution-time limits, because they are wall-clock based.
- Lock contention, allocator behavior under concurrency, and I/O latency. Callgrind counts instructions, not time. Changes that only move work to another thread, such as a writer-thread deferral, show up as a builder-thread decrease, which is the latency-relevant direction.
- Isthmus-and-later header work (withdrawals root) and blob fields; the synthetic chain activates only L1 forks through Cancun.
- For the native builder: the `BasicPayloadJob` around `try_build` (interval scheduling, finalization requests, cached reads, and the parallel state-root task; the benchmark computes the state root synchronously), transactions that arrive during the pass, and the pre-Denim mode in which both builders build every payload and the native builder rebuilds on each interval.
- Backlog growth across blocks. The benchmarks measure per-block cost at a fixed backlog size; long-term trends in production build time need production monitoring.
