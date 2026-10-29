# Hot-Path Performance Review

This guide covers performance on Base hot paths: the loops that run per candidate
transaction, per submitted transaction, per flashblock, or per block, before and
after the Denim network upgrade. Authors use
it when changing that code, and reviewers (human and LLM) apply it to PRs that
touch it. A diff rarely shows how often the changed code runs, so establish the
call frequency before judging cost.

## Hot-path registry

Frequencies are mainnet orders of magnitude. Candidates far outnumber included
transactions: the builder re-evaluates pooled transactions on every build, and
rejected, deferred, and parked transactions are revisited.

Denim replaces flashblocks with native 200 ms blocks built by the native payload
builder. Code that only the flashblocks builder runs stops mattering at Denim.
The native builder, block execution, and per-block work run about 10 times as
often as they do today. Review native builder changes at their post-Denim rate,
even while mainnet still runs flashblocks.

| Path | Entry point | Runs per | Mainnet rate |
|---|---|---|---|
| Native builder selection and execution (production after Denim) | `crates/execution/payload/src/builder.rs` `execute_best_transactions` loop body, plus everything it calls, including resource metering and validity predicates | candidate transaction, per 200 ms block | ~10^9 candidates/day vs ~10^7 included |
| Native block assembly | `crates/execution/payload/src/builder.rs` `build_payload`/`try_build`, sealing, state root, `crates/builder/multiplex` block stream | block | ~4×10^5/day after Denim |
| Flashblock transaction selection and execution (until Denim) | `crates/builder/core/src/flashblocks/context.rs` `execute_best_transactions` loop body, plus everything it calls | candidate transaction, per flashblock | ~10^9 candidates/day vs ~10^7 included |
| Best-transaction iteration and parking (both builders) | `crates/execution/txpool/src/best.rs` (`MergeBestTransactions::next`, `pop_best`, `mark_invalid`), `crates/execution/txpool/src/two_d_nonce_pool.rs` (`BestTwoDTransactions::next`, `mark_invalid`), `crates/execution/payload/src/parkable.rs`, `crates/builder/core` parkable iterators, `crates/execution/txpool` ordering and validity predicates | candidate transaction (pop or rejection) | ~10^9 candidates/day |
| Best-transaction snapshot construction (both builders) | `crates/execution/txpool/src/pool.rs` (`best_transactions`, `best_transactions_with_attributes`, `merged_best_transactions`), reth's pending snapshot, `crates/execution/txpool/src/two_d_nonce_pool.rs` (`TwoDNoncePool::best_transactions`, `BestTwoDTransactions::new`) | flashblock (~10/block), or 200 ms block after Denim | ~4×10^5/day |
| Transaction pool admission and lane/canonical maintenance | `crates/execution/txpool/src/pool.rs` (`add_transaction*`, `remove_transactions*`, `prune_transactions`, `on_canonical_state_change`), `crates/execution/txpool/src/two_d_nonce_pool.rs` (`insert_validated*`, `set_lane_cursor`, `refresh_lane_indexes`, `evict_over_capacity`), validation, RPC `eth_sendRawTransaction` | submitted, mined, or re-anchored transaction | ~10^8/day |
| Pool occupancy, p2p, and bounded queries | `crates/execution/txpool/src/pool.rs` (`pool_size`, `pending_transactions_max`, `pooled_transaction_hashes_max`, `pooled_transactions_max`, `retain_unknown`, `get_pooled_transaction_elements`) | metrics scrape, p2p announcement, or RPC query | continuous under load (`10^4–10^5` pooled txs) |
| Flashblock publish (until Denim) | `crates/builder/publish` | flashblock (10 per block) | ~4×10^5/day |
| Block execution and state root | `crates/execution/*` executor, trie | block and touched account/slot | ~4×10^4 blocks/day, ~4×10^5 after Denim |

Code is on a hot path when it runs inside one of these loops, including through
closures, trait impls, `Drop`, metrics recorders, and tracing layers.

## Cost model

Mainnet builds each flashblock in a ~200 ms window. After Denim the sequencer
requests each native block about 150 ms into its 200 ms slot, and the state root
and import must fit in what remains. Per-candidate work multiplies:
1 µs per candidate at 10^9 candidates/day is ~17 CPU-minutes per day spent inside
the latency-critical build loop and directly steals transactions from blocks.

## Review procedure

1. **Trace call frequency and population scale from the registry above.** Read the full body of every changed function and of any *unchanged* callee or data structure whose call rate or input population size the PR increases (for example, routing a `10^4–10^5` transaction pool into a structure originally written for a small subpool, or calling an existing helper inside a hotter loop).
2. **Multiply inner-loop work by outer-loop frequency.** Do not accept comments or PR descriptions claiming a set is "small in steady state" or "only ready items"; determine what bounds the collection in the worst case (such as full blocks, price gaps, nonce gaps, or bursty single-sender lanes).
3. **Audit PR benchmarks before trusting their numbers.** Verify that benchmark fixtures populate worst-case states: released/bridged/deferred candidates that *do not* immediately outbid the block cutoff (so waiting/ready sets accumulate across pops), multi-transaction lanes (`m > 1`), and all cross-store directions. Flag benchmarks whose priority or nonce distributions accidentally collapse `O(N · L)` or `O(m²)` behavior into `O(1)`.

## What to flag

On a hot path, flag any per-unit work—whether newly written or inherited from existing code now called at a higher rate or larger population—that block building or pool admission does not need, with the per-unit cost and the multiplier:

- **Linear scans inside per-candidate iteration (`O(N · L)` per build):** `BestTransactions::next`, `pop_best`, and `mark_invalid` run `N` times per flashblock/block. Scanning ready, held, deferred, or bridged lanes linearly on each pop costs `O(N · L)` when low-tip ready lanes remain unpicked across pops. Require a `BinaryHeap` or priority index (`O(log L)` per pop) and verify `mark_invalid` clears or demotes cross-store successors across *every* store's buffered head and waiting set.
- **Per-flashblock / per-block deep copies and index rebuilds (`O(P)` per build):** `best_transactions` / `BestTwoDTransactions::new` runs every 200 ms. Flag walking all lanes or pooled transactions to clone `Arc<ValidPoolTransaction>`s into fresh `Vec`s or rebuild `HashMap`/`BinaryHeap` indexes per snapshot when the store can hold `10^4–10^5` transactions. Maintain head/priority indexes incrementally on admission and removal.
- **Quadratic single-lane admission or maintenance (`O(m²)`):** Flag calling `O(m)` lane-traversal helpers (such as `consecutive_pending_len()` or `queued_transactions()`) multiple times per insert/remove on a lane of length `m`, or allocating temporary `HashSet<TxHash>`s in `set_lane_cursor` when `next_nonce..pending_end` range math suffices.
- **Clone-to-count and clone-to-truncate in pool queries (`O(P)` allocations):** Flag calling `pending_transactions()`, `queued_transactions()`, or `all_transactions()` (which allocate a `Vec` and clone every `Arc` in the store) just to sum byte sizes (`pool_size`), count entries, or `.take(max)` (`*_max` methods). Use tracked counters (`pool.size()`) or bounded iterators.
- **Serialization or formatting:** `serde_json`, `to_value`, `format!`, `to_string`, `{:#x}` hex encoding, building `Map<String, Value>`.
- **Heap allocation and cloning:** `Vec`/`String`/`Map`/`HashSet` construction per candidate or admission, `clone()` of non-`Copy` data, `Arc` churn, boxing closures.
- **Observability done eagerly:** events, logs, and metrics whose fields are built before the level or enabled check, and metrics labels allocated per call. Deferring to a background thread still pays the allocation and moves the data; prefer not emitting per-candidate records.
- **Metric handle lookups per unit:** a labeled `counter!`/`histogram!` call, or a generated accessor such as `BuilderMetrics::foo_total(label)`, builds a key and looks it up in the recorder on every call. Resolve handles once per loop.
- **Locks, channel sends, syscalls, file or network I/O.**
- **Repeated work:** recomputing per candidate what is fixed per flashblock or block, re-emitting identical records for the same transaction.
- **Super-linear behavior** in the number of candidates, lanes, parked transactions, or predicates.

**Evaluate default-off features as enabled.** Production enables features that
default off in tests, benchmarks, and devnet (for example
`--builder.transaction-events.enabled`, or the native builder's resource metering
schedule, `--payload.resource-metering-schedule`). An early return when a feature is
disabled hides the cost from every pre-production measurement. Review the
enabled branch at production frequency.

A finding states: the hot path and its multiplier, the added per-unit work, an
estimated per-unit cost, and a cheaper alternative (move it out of the loop,
aggregate per flashblock, sample, or drop it). Ask for a benchmark of the
enabled path when the cost is unclear: `crates/builder/core/benches/build_loop.rs`
drives `execute_best_transactions` over a mainnet-shaped block, and
`BUILD_LOOP_PROFILE=production` turns on the observability production enables.
`crates/builder/core/benches/tx_selection.rs` covers iteration, parking, and the
predicate index. Both drive the flashblocks builder only. For the native builder,
`crates/execution/payload/benches/resource_metering.rs` covers the per-transaction
resource-metering checks; the native `execute_best_transactions` loop has no
end-to-end benchmark yet.

## What not to flag

Cold paths (startup, config, per-block work that is O(1) relative to execution;
remember that blocks are 10 times as frequent after Denim),
micro-optimizations without a hot-path multiplier, and style.
