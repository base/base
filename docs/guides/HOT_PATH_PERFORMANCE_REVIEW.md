# Hot-Path Performance Review

Reviewers (human and LLM) use this guide on PRs that touch code reachable from a
hot path. A diff rarely shows how often the changed code runs, so the reviewer
must establish the call frequency before judging cost.

## Hot-path registry

Frequencies are mainnet orders of magnitude. Candidates far outnumber included
transactions: the builder re-evaluates pooled transactions on every flashblock,
and rejected, deferred, and parked transactions are revisited.

| Path | Entry point | Runs per | Mainnet rate |
|---|---|---|---|
| Flashblock transaction selection and execution | `crates/builder/core/src/flashblocks/context.rs` `execute_best_transactions` loop body, plus everything it calls | candidate transaction, per flashblock | ~10^9 candidates/day vs ~10^7 included |
| Native builder selection | `crates/execution/payload/src/builder.rs` best-transaction loop | candidate transaction | same order as above |
| Best-transaction iteration and parking | `crates/builder/core` parkable iterators, `crates/execution/txpool` ordering and validity predicates | candidate transaction | same order as above |
| Transaction pool admission | `crates/execution/txpool` validation, RPC `eth_sendRawTransaction` | submitted transaction | ~10^8/day |
| Flashblock publish | `crates/builder/publish` | flashblock (10 per block) | ~4×10^5/day |
| Block execution and state root | `crates/execution/*` executor, trie | block and touched account/slot | ~4×10^4 blocks/day |

Code is on a hot path when it runs inside one of these loops, including through
closures, trait impls, `Drop`, metrics recorders, and tracing layers.

## Cost model

Mainnet builds each flashblock in a ~200 ms window. Per-candidate work multiplies:
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
  before the level or enabled check, metrics labels allocated per call, or
  per-unit metric handle lookups. Deferring to a background thread still pays the
  allocation and moves the data; prefer not emitting per-candidate records.
- Locks, channel sends, syscalls, file or network I/O.
- Repeated work: recomputing per candidate what is fixed per flashblock or block,
  re-emitting identical records for the same transaction.
- Super-linear behavior in the number of candidates, parked transactions, or
  predicates.

**Evaluate default-off features as enabled.** Production enables features that
default off in tests, benchmarks, and devnet (for example
`--builder.transaction-events.enabled`). An early return when a feature is
disabled hides the cost from every pre-production measurement. Review the
enabled branch at production frequency.

A finding states: the hot path and its multiplier, the added per-unit work, an
estimated per-unit cost, and a cheaper alternative (move it out of the loop,
aggregate per flashblock, sample, or drop it). Ask for a benchmark of the
enabled path when the cost is unclear.

## What not to flag

Cold paths (startup, config, per-block work that is O(1) relative to execution),
micro-optimizations without a hot-path multiplier, and style.
