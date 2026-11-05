# Hot-Path Performance Review

This guide covers performance on Base hot paths: the loops that run per candidate
transaction, per submitted transaction, or per block. Authors use
it when changing that code, and reviewers (human and LLM) apply it to PRs that
touch it. A diff rarely shows how often the changed code runs, so establish the
call frequency before judging cost.

## Hot-path registry

Frequencies are mainnet orders of magnitude. Candidates far outnumber included
transactions: the builder re-evaluates pooled transactions on every build, and
rejected, deferred, and parked transactions are revisited.

Since the Denim network upgrade the sequencer builds native 200 ms blocks with
the native payload builder, so the builder, block execution, and per-block work
run about 10 times as often as they did with 2 s blocks.

| Path | Entry point | Runs per | Mainnet rate |
|---|---|---|---|
| Native builder selection and execution | `crates/execution/payload/src/builder.rs` `execute_best_transactions` loop body, plus everything it calls, including resource metering and validity predicates | candidate transaction, per 200 ms block | ~10^9 candidates/day vs ~10^7 included |
| Native block assembly | `crates/execution/payload/src/builder.rs` `build_payload`/`try_build`, sealing, state root | block | ~4×10^5/day |
| Best-transaction iteration and parking | `crates/execution/payload/src/parkable.rs`, `crates/execution/txpool` ordering and validity predicates | candidate transaction | same order as above |
| Best-transaction snapshot | `crates/execution/txpool/src/pool.rs` `merged_best_transactions`, reth's pending snapshot, `TwoDNoncePool::best_transactions` for the sidecar and the validity pool | block | ~4×10^5/day; `cargo bench -p base-execution-txpool --bench best_transactions` |
| Transaction pool admission | `crates/execution/txpool` validation, RPC `eth_sendRawTransaction` | submitted transaction | ~10^8/day; `cargo bench -p base-execution-txpool --bench admission` |
| Block execution and state root | `crates/execution/*` executor, trie | block and touched account/slot | ~4×10^5 blocks/day |

Code is on a hot path when it runs inside one of these loops, including through
closures, trait impls, `Drop`, metrics recorders, and tracing layers.

## Cost model

The sequencer requests each native block about 150 ms into its 200 ms slot, and the state root
and import must fit in what remains. Per-candidate work multiplies:
1 µs per candidate at 10^9 candidates/day is ~17 CPU-minutes per day spent inside
the latency-critical build loop and directly steals transactions from blocks.

## What to flag

On a hot path, flag any added per-unit work that block building does not need,
with the per-unit cost and the multiplier:

- Serialization or formatting: `serde_json`, `to_value`, `format!`, `to_string`,
  `{:#x}` hex encoding, building `Map<String, Value>`.
- Heap allocation and cloning: `Vec`/`String`/`Map` construction, `clone()` of
  non-`Copy` data, `Arc` churn, boxing closures.
- Observability done eagerly: events, logs, and metrics whose fields are built
  before the level or enabled check, and metrics labels allocated per call.
  Deferring to a background thread still pays the allocation and moves the data;
  prefer not emitting per-candidate records.
- Metric handle lookups per unit: a labeled `counter!`/`histogram!` call, or a
  generated accessor such as `BuilderMetrics::foo_total(label)`, builds a key and
  looks it up in the recorder on every call. Resolve handles once per loop.
- Locks, channel sends, syscalls, file or network I/O.
- Repeated work: recomputing per candidate what is fixed per block,
  re-emitting identical records for the same transaction.
- Super-linear behavior in the number of candidates, parked transactions, or
  predicates.

**Evaluate default-off features as enabled.** Production enables features that
default off in tests, benchmarks, and devnet (for example
`--builder.transaction-events.enabled`, or the native builder's resource metering
schedule, `--payload.resource-metering-schedule`). An early return when a feature is
disabled hides the cost from every pre-production measurement. Review the
enabled branch at production frequency.

A finding states: the hot path and its multiplier, the added per-unit work, an
estimated per-unit cost, and a cheaper alternative (move it out of the loop,
aggregate per block, sample, or drop it). Ask for a benchmark of the
enabled path when the cost is unclear:
`crates/execution/payload/benches/resource_metering.rs` covers the per-transaction
resource-metering checks, and `native_candidate_scan.rs` and `native_build_loop.rs`
drive the native `execute_best_transactions` loop. Wall-clock
numbers from these benches drift with machine load, so compare two commits with
`etc/scripts/local/bench_abba.py`, which builds both and runs them interleaved
(A B B A) and reports the per-arm medians of criterion's per-run estimates.

Deterministic instruction-count (`*_iai.rs`) benchmarks cover the native builder
(`crates/execution/payload`: candidate scan, validity predicates, multi-block build
loop), block execution and assembly (`crates/execution/evm`), and pool admission and
builder inserts (`crates/execution/txpool`). The `bench-iai.yml` workflow reports their
base-vs-head counts on every PR; run one locally with
`cargo bench -p <crate> --bench <name>_iai` (needs Valgrind and
`iai-callgrind-runner` 0.16.1).

## What not to flag

Cold paths (startup, config, per-block work that is O(1) relative to execution;
remember that 200 ms blocks make per-block work 10 times as frequent as 2 s blocks did),
micro-optimizations without a hot-path multiplier, and style.
