# Performance Autopilot Journal

This file records each autonomous performance run.

Entry format

## YYYY-MM-DD HH:MM UTC
Focus:
Hypothesis:
Commands:
Results:
Next:

## 2026-04-26 14:01 UTC
Focus: bootstrap performance autopilot
Hypothesis: the repo already contains enough service hotspots, metrics, load tests, and benchmark hooks to start an autonomous optimization loop without guessing blindly.
Commands:
- inspected consensus, batcher, zk service, and shared benchmarking infrastructure
- created dedicated worktree `/home/refcell/dev/base-perf-autopilot`
Results:
- consensus hotspots identified around sequencer build/seal, engine request processing, derivation, provider RPC/cache behavior, and gossip paths
- batcher hotspots identified around driver scheduling, encoder/compression, blob packing, recent-tx startup scan, and source polling/catchup
- zk hotspots identified around witness generation, status polling, repeated GetProof sync calls, session round-trips, and proxy rate limiting
- reusable benchmarking infrastructure exists in `crates/infra/load-tests` plus several criterion benches elsewhere in the repo
Next:
- start with the narrowest measurable improvement in one hotspot area, likely batcher encoder/submission or zk service polling behavior

## 2026-04-26 18:32 UTC
Focus: `base-batcher-encoder` DA backlog accounting in `BatchEncoder::da_backlog_bytes()`.
Hypothesis: the backlog getter is on the batcher throttle path and should not rescan every unencoded block/transaction; caching the pending DA bytes should turn reads from O(n) into O(1) while preserving exact behavior.
Commands:
- `cargo test -p base-batcher-encoder`
- `cargo bench -p base-batcher-encoder --bench da_backlog -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 20`
- `cargo clippy -p base-batcher-encoder --tests --benches -- -D warnings`
- `cargo fmt --all`
Results:
- added a cached `da_backlog_bytes: u64` field to `BatchEncoder`, updating it on `add_block`, successful single/span encoding, and `reset()` so `da_backlog_bytes()` is now O(1)
- added `test_da_backlog_cache_tracks_encoding_lifecycle` to verify cache correctness through add/encode/reset transitions; full crate tests pass (`60 passed`)
- added a new Criterion bench at `crates/batcher/encoder/benches/da_backlog.rs` covering `4096_blocks_pending` and `4096_blocks_half_encoded`
- fixed the bench harness so clippy passes and benchmark calls are not trivially constant-folded via `black_box`
- post-change benchmark results: `4096_blocks_pending` = `202.18 ps .. 203.75 ps`, `4096_blocks_half_encoded` = `201.22 ps .. 201.49 ps`, confirming constant-time reads regardless of backlog depth/encoded cursor position
Next:
- watch for any follow-up review feedback on whether additional encoder counters or a comparative regression benchmark against the old linear scan would be useful

## 2026-04-26 20:42 UTC
Focus: `base-consensus-node` derivation finalizer drain path in `L2Finalizer::try_finalize_next()`.
Hypothesis: when finalizing after a deep derivation backlog, draining finalized epochs with `BTreeMap::retain` does unnecessary full-map scanning; replacing it with `BTreeMap::split_off` should keep only future epochs in O(log n) tree work plus moved survivors, reducing the hot-path cost while preserving semantics.
Commands:
- `cargo bench -p base-consensus-node --bench finalizer -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 20`
- `cargo test -p base-consensus-node actors::derivation::finalizer:: -- --nocapture --test-threads=1`
- `cargo clippy -p base-consensus-node --tests --benches --no-deps -- -D warnings`
- `cargo fmt --all`
Results:
- added a new Criterion bench at `crates/consensus/service/benches/finalizer.rs` covering `enqueue_for_finalization`, `try_finalize_next` on a `4096`-entry queue, and an empty-queue miss case
- replaced the finalization drain in `L2Finalizer::try_finalize_next()` with a helper backed by `BTreeMap::split_off`, and handled `u64::MAX` explicitly to avoid overflow when computing the first surviving epoch
- added `max_l1_finalization_drains_without_overflow` to lock in the overflow edge case
- initial baseline bench before the code change measured `4096_entries_finalize_half` at `40.104 µs .. 42.834 µs`, `4096_unique_l1_epochs` at `107.75 µs .. 109.15 µs`, and `empty_queue_miss` at `4.5253 ns .. 4.7974 ns`
- post-change re-run measured `4096_entries_finalize_half` at `10.928 µs .. 14.483 µs`, roughly a 3-4x improvement on the drain path; `4096_unique_l1_epochs` stayed effectively flat at `103.65 µs .. 105.15 µs`, and `empty_queue_miss` stayed flat at `6.5262 ns .. 6.6165 ns` on the confirming run
- focused finalizer tests passed (`9 passed`)
- full `cargo clippy -p base-consensus-node --tests --benches -- -D warnings` is currently blocked by pre-existing lint failures in dependency crate `base-consensus-disc`, so validation used `--no-deps` and passed for the touched crate
Next:
- watch PR feedback on whether the finalizer bench should grow a larger survivor-heavy case (for example, finalizing 1 block out of a much larger queue) to characterize `split_off` behavior under different retained-tail sizes

## 2026-04-26 22:49 UTC
Focus: `base-consensus-node` finalizer benchmarking coverage for survivor-heavy drain cases in `L2Finalizer::try_finalize_next()`.
Hypothesis: the prior finalizer bench only measured the half-drain case, leaving a gap for the likely worst retained-tail shape after the `split_off` optimization; adding a `finalize_first` benchmark should quantify the cost when almost the entire queue survives.
Commands:
- `cargo bench -p base-consensus-node --bench finalizer -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 20`
- edited `crates/consensus/service/benches/finalizer.rs` to add `4096_entries_finalize_first`
- `cargo bench -p base-consensus-node --bench finalizer -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 20`
- `cargo test -p base-consensus-node actors::derivation::finalizer:: -- --nocapture --test-threads=1`
- `cargo clippy -p base-consensus-node --tests --benches --no-deps -- -D warnings`
- `cargo fmt --all`
Results:
- baseline before the bench edit confirmed the existing post-optimization behavior: `4096_entries_finalize_half` = `11.155 µs .. 14.292 µs`, `4096_unique_l1_epochs` = `105.13 µs .. 107.81 µs`, `empty_queue_miss` = `6.5632 ns .. 6.8584 ns`
- added a new survivor-heavy Criterion case, `4096_entries_finalize_first`, without changing production logic
- the new benchmark measured `4096_entries_finalize_first` at `10.206 µs .. 11.149 µs`, showing the `split_off` drain remains in the same low-`10 µs` band even when `4095/4096` entries survive
- confirming run kept `4096_entries_finalize_half` in the same range at `11.880 µs .. 16.408 µs`; `empty_queue_miss` stayed flat at `6.5506 ns .. 6.8324 ns`
- focused finalizer tests still passed (`9 passed`)
- `cargo clippy -p base-consensus-node --tests --benches --no-deps -- -D warnings` passed again; full clippy without `--no-deps` remains blocked by pre-existing dependency lints outside the touched crate
Next:
- if the finalizer is revisited, add a larger matrix of retained-tail sizes (for example finalize-at-1, finalize-at-1/4, finalize-at-1/2, finalize-at-3/4) to characterize how `split_off` scales with survivor count and to catch future regressions in queue-shape sensitivity

## 2026-04-27 01:10 UTC
Focus: `base-batcher-service` recent transaction startup scan in `RecentTxScanner::highest_submitted_l2_block()`.
Hypothesis: the startup scan should not rescan every buffered channel after each L1 block when only channels touched by frames in the current block can transition to ready; restricting the drain to touched channel IDs should reduce unnecessary map scans, especially when the buffered channel set is large and the current block only advances a small subset.
Commands:
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 20`
- added a focused Criterion bench at `crates/batcher/service/benches/recent_txs.rs`
- changed `RecentTxScanner` to track per-block `touched_channel_ids` and drain only those ready channels
- added `drain_ready_channels_only_checks_touched_ids`
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- extracted two public benchmark hooks: `drain_ready_channels` for the touched-only path and `drain_all_ready_channels` for the old full-scan baseline, without changing `decode_channel` behavior
- replaced the per-block `channels.iter().filter(|(_, ch)| ch.is_ready())` full scan with touched-ID draining in `highest_submitted_l2_block()`
- added a regression test proving untouched ready channels remain buffered, touched incomplete channels stay buffered, and touched ready channels are decoded and removed when their ID is supplied
- added `criterion` bench coverage for three shapes: fully touched mixed channels, fully touched incomplete channels, and sparse touched channels against a larger buffered set
- validation passed: focused tests `6 passed`; `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings` passed
- benchmark results after the change:
  - `baseline_scan_all_with_4096_ready_and_4096_incomplete`: `7.8886 ms .. 8.4072 ms`
  - `4096_touched_ready_among_8192_channels`: `7.8312 ms .. 8.2868 ms` (effectively flat because ready-channel decode dominates)
  - `baseline_scan_all_with_8192_incomplete`: `391.27 µs .. 672.34 µs`
  - `4096_touched_incomplete_among_8192_channels`: `427.93 µs .. 511.84 µs` (same rough band)
  - `baseline_scan_all_with_64_touched_ready_among_8192_channels`: `513.58 µs .. 720.29 µs`
  - `64_touched_ready_among_8192_channels`: `507.07 µs .. 833.40 µs` (noisy, effectively flat)
  - `baseline_scan_all_with_64_touched_incomplete_among_8192_channels`: `396.74 µs .. 424.37 µs`
  - `64_touched_incomplete_among_8192_channels`: `366.17 µs .. 382.39 µs` (~8-10% lower in the sparse no-decode case, matching the intended avoided-scan scenario)
Next:
- if this path is revisited, grow the benchmark matrix around frame fan-out and touched-ID cardinality so the crossover point between touched-only draining and full-map scanning is explicit, especially for startup scans with many buffered but mostly untouched channels

## 2026-04-27 03:21 UTC
Focus: `base-batcher-service` touched-channel deduplication inside `RecentTxScanner::highest_submitted_l2_block()`.
Hypothesis: after moving the startup scan to touched-only draining, per-block deduplication via `Vec::contains` is still O(k²) in the number of parsed frames; replacing it with a small reusable tracker backed by `HashSet` + ordered `Vec` should materially reduce the bookkeeping cost for high fan-out blocks without changing drain semantics.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- edited `crates/batcher/service/src/recent_txs.rs` to add `TouchedChannelTracker` and use it from `highest_submitted_l2_block()`
- extended `crates/batcher/service/benches/recent_txs.rs` with comparative touched-ID tracking microbenches for the old `Vec::contains` path vs. the new tracker
- `cargo fmt --all`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- added a public `TouchedChannelTracker` type, re-exported from `lib.rs`, that preserves first-seen channel order while deduplicating in O(1)-average membership checks via `HashSet`
- switched `highest_submitted_l2_block()` from ad hoc `Vec::contains` deduplication to `TouchedChannelTracker::record`, keeping the touched-only drain API unchanged
- added `touched_channel_tracker_deduplicates_and_preserves_first_seen_order` to lock in ordering and dedup behavior alongside the existing drain regression test
- added focused Criterion coverage that compares the old vector scan against the new tracker for `4096` unique touched IDs and `4096` frames spread across `512` unique channel IDs
- validation passed: focused tests `7 passed`; `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings` passed
- touched-ID tracking benchmark results after the change:
  - `baseline_vec_scan_4096_unique_frame_channel_ids`: `1.9804 ms .. 2.0371 ms`
  - `hashset_tracker_4096_unique_frame_channel_ids`: `46.993 µs .. 47.127 µs` (~42x lower)
  - `baseline_vec_scan_4096_frames_across_512_unique_channel_ids`: `272.70 µs .. 278.45 µs`
  - `hashset_tracker_4096_frames_across_512_unique_channel_ids`: `43.241 µs .. 43.367 µs` (~6.3x lower)
- existing drain-path benches stayed in the same rough bands as the prior run, so this iteration primarily improved per-block touched-ID bookkeeping rather than decode-heavy channel draining
Next:
- if startup scan latency is revisited again, add an end-to-end block-parsing bench that combines frame parsing, touched-ID tracking, and touched-only draining so the next iteration can quantify where bookkeeping stops mattering relative to channel decode cost

## 2026-04-27 05:28 UTC
Focus: `base-batcher-service` per-block tracker reuse inside `RecentTxScanner::highest_submitted_l2_block()`.
Hypothesis: after replacing touched-channel deduplication with `TouchedChannelTracker`, the startup scan still allocates a fresh tracker per L1 block; reusing one tracker across blocks should shave a little more bookkeeping overhead off the scan without changing touched-ID order or drain semantics.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- edited `crates/batcher/service/src/recent_txs.rs` to let `TouchedChannelTracker` clear/reset its storage and reused a single tracker across the scan loop
- extended `crates/batcher/service/benches/recent_txs.rs` with comparative microbenches for fresh-allocating vs. reused tracker bookkeeping
- `cargo fmt --all`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- added `TouchedChannelTracker::clear` and `reset_with_capacity`, then moved `highest_submitted_l2_block()` to reuse one tracker across the whole block scan instead of allocating a fresh `HashSet` + `Vec` pair for every fetched L1 block
- added `touched_channel_tracker_reset_allows_reuse_after_clear` to lock in reuse semantics and prove dedup/order behavior survives resets; focused tests now pass (`8 passed`)
- added two new Criterion cases to compare the old fresh-allocation tracker path against the reused tracker path directly
- end-to-end drain-path benches stayed noisy and effectively flat, which matches prior runs that showed decode work dominates there
- comparative bookkeeping microbench results on the confirming run:
  - `hashset_tracker_4096_unique_frame_channel_ids`: `48.533 µs .. 48.898 µs`
  - `reused_hashset_tracker_4096_unique_frame_channel_ids`: `48.225 µs .. 48.270 µs` (~0.9% lower median)
  - `hashset_tracker_4096_frames_across_512_unique_channel_ids`: `44.627 µs .. 44.936 µs`
  - `reused_hashset_tracker_4096_frames_across_512_unique_channel_ids`: `44.311 µs .. 44.498 µs` (~0.8% lower median)
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings` passed
Next:
- if the startup scan is revisited again, add a block-level benchmark that includes frame parsing plus touched-ID tracking so allocator reuse can be measured in a shape closer to real RPC-fetched blocks, not just the isolated tracker microbench

## 2026-04-27 07:36 UTC
Focus: `base-batcher-service` ready-channel lookup cost inside `RecentTxScanner::drain_ready_channels()`.
Hypothesis: after touched-only draining and touched-ID tracker improvements, the remaining per-channel bookkeeping might still benefit from avoiding the separate `HashMap::get` + `HashMap::remove` sequence; a focused lookup microbench should show whether an `entry`-based path is worth pursuing before touching production code.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- temporarily changed `RecentTxScanner::drain_ready_channels()` to use `HashMap::entry` and re-ran focused validation/benchmarks
- reverted the production change after measurement showed no win
- extended `crates/batcher/service/benches/recent_txs.rs` with a new `ready_channel_lookup` Criterion group that compares `get`-based readiness checks against `entry`-based lookups on the touched-only path
- `cargo fmt --all`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
Results:
- added benchmark-only coverage for the ready-check bookkeeping gap without changing production logic; the bench file is now the only code delta for this run
- the focused lookup microbench showed `entry` is not a clear win here and is often worse on the important no-ready and sparse-ready shapes, so the production `get` + `remove` implementation was intentionally left unchanged
- confirming lookup results with the new benchmark group:
  - `baseline_get_4096_touched_incomplete_among_8192_channels`: `411.61 µs .. 447.80 µs`
  - `entry_api_4096_touched_incomplete_among_8192_channels`: `441.73 µs .. 593.54 µs` (slower / noisier)
  - `baseline_get_64_touched_ready_among_8192_channels`: `395.70 µs .. 751.97 µs`
  - `entry_api_64_touched_ready_among_8192_channels`: `378.14 µs .. 418.30 µs` (not enough evidence of a durable win given the baseline noise)
- existing drain-path benches remained in the same rough bands, with decode-heavy cases still dominating end-to-end startup scan cost
- focused tests still passed (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings` passed
Next:
- if this startup scan path is revisited again, benchmark frame parsing plus touched-only draining together so the next candidate optimization is chosen from a more end-to-end block-level profile instead of another tiny lookup tweak

## 2026-04-27 09:48 UTC
Focus: `base-batcher-service` recent-tx startup scan block-level benchmarking coverage.
Hypothesis: the existing microbenches proved touched-ID bookkeeping wins in isolation, but the next useful measurement layer is a block-level harness that includes version-0 frame parsing, channel mutation, and draining together; this should show whether the touched-only startup-scan changes still matter once more realistic per-block work is included.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with a new `process_block` Criterion group that replays encoded version-0 frame payloads into the same `Frame::parse_frames` + channel update + drain flow used by `RecentTxScanner`
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- added benchmark-only helpers that synthesize encoded transaction payloads and compare the old per-block `Vec::contains` + full-map drain strategy against the current tracker-backed touched-only drain path without requiring RPC I/O
- the new `process_block` group closes the gap between tiny bookkeeping microbenches and the noisier end-to-end drain benchmarks by measuring parsing, channel insertion, touched-ID tracking, and draining together on a single fixture block
- new block-level benchmark results:
  - `baseline_vec_scan_all_4096_ready_unique_channels_from_empty`: `9.5927 ms .. 9.6277 ms`
  - `tracker_touched_only_4096_ready_unique_channels_from_empty`: `7.6659 ms .. 7.8739 ms` (~19% lower median)
  - `baseline_vec_scan_all_64_incomplete_touches_among_8192_buffered_channels`: `24.384 µs .. 25.826 µs`
  - `tracker_touched_only_64_incomplete_touches_among_8192_buffered_channels`: `7.2882 µs .. 10.650 µs` (roughly 2.5-3x lower despite some noise)
- this confirms the prior touched-only + tracker work still produces a measurable win once frame parsing and channel mutation are included, even though the decode-heavy drain benchmarks remain noisy on some shapes
- focused validation passed again: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Next:
- if this path is revisited again, add a multi-block benchmark that reuses the same tracker across successive synthetic L1 blocks so allocator reuse and survivor-heavy buffered-channel sets can be measured in a shape even closer to the real startup scan loop

## 2026-04-27 11:57 UTC
Focus: `base-batcher-service` recent-tx startup scan multi-block benchmarking coverage for tracker reuse.
Hypothesis: the prior single-block harness still hid most of the small tracker-reuse benefit; a multi-block benchmark that keeps the buffered channel map alive across several synthetic L1 blocks should make the fresh-vs-reused tracker choice measurable in a shape closer to the real startup scan loop.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with new multi-block payload builders plus a `process_blocks` Criterion group that compares fresh tracker allocation per block against `TouchedChannelTracker::reset_with_capacity` reuse across eight successive synthetic blocks
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-batcher-service --bench recent_txs -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
Results:
- kept production logic unchanged and added benchmark-only coverage for the exact next gap identified in the previous run: persistent buffered channels across multiple synthetic blocks with touched-only draining after each block
- the new `process_blocks` group isolates tracker lifecycle cost better than the single-block harness by letting channels persist while replaying `8 × 4096` incomplete touches, which is closer to a survivor-heavy startup scan
- new multi-block benchmark results:
  - `fresh_tracker_8_blocks_4096_incomplete_touches_each_among_persistent_channels`: `3.8344 ms .. 4.2619 ms`
  - `reused_tracker_8_blocks_4096_incomplete_touches_each_among_persistent_channels`: `3.7320 ms .. 3.8966 ms` (~6.4% lower median)
- confirming run also kept the earlier block-level signal intact: `baseline_vec_scan_all_4096_ready_unique_channels_from_empty` = `9.5838 ms .. 9.7675 ms`, `tracker_touched_only_4096_ready_unique_channels_from_empty` = `7.6493 ms .. 7.8187 ms`
- focused validation passed again: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Next:
- if this startup-scan path is revisited again, extend the multi-block harness with channels that become ready only on later blocks so the benchmark covers both tracker reuse and the eventual decode/drain transition inside a persistent survivor-heavy channel set

## 2026-04-27 14:07 UTC
Focus: `base-batcher-service` recent-tx startup scan delayed-ready multi-block benchmark coverage.
Hypothesis: the prior multi-block harness only measured incomplete survivor-heavy channels, so it still underrepresented the moment when touched channels finally become ready and trigger decode/drain work; a delayed-ready multi-block fixture should better capture the durable benefit of touched-only draining once channels finish on a later block.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with `split_frame_data_across_blocks`, `multi_block_ready_transition_tx_payloads`, `process_blocks_with_vec_tracking_and_full_scan`, and a new `process_blocks_ready_transition` Criterion group
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_ready_transition -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- the pre-change check confirmed the earlier incomplete-only `process_blocks` harness still does not show a durable tracker-reuse win on this machine (`fresh_tracker_8_blocks_4096_incomplete_touches_each_among_persistent_channels`: `3.4449 ms .. 3.7434 ms`; `reused_tracker_8_blocks_4096_incomplete_touches_each_among_persistent_channels`: `3.8477 ms .. 4.3095 ms`)
- added benchmark-only delayed-ready coverage where `1024` channels receive four split frames across four synthetic blocks and only become ready on the final block, keeping production logic unchanged
- the new delayed-ready harness showed the touched-only path still materially beats the old full-map drain once readiness and decode happen later in a persistent channel set:
  - `baseline_vec_scan_all_4_blocks_1024_channels_ready_on_final_block`: `2.6940 ms .. 2.7362 ms`
  - `fresh_tracker_4_blocks_1024_channels_ready_on_final_block`: `2.3213 ms .. 2.3388 ms` (~14.1% lower median)
  - `reused_tracker_4_blocks_1024_channels_ready_on_final_block`: `2.3446 ms .. 2.3634 ms` (~12.8% lower median, slightly slower than fresh allocation in this shape)
- focused validation passed again: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Next:
- if this path is revisited again, vary the delayed-ready matrix (for example channel count, block count, and percentage of channels completing on each block) so the crossover between decode cost, touched-only draining, and tracker reuse is explicit before attempting another production optimization

## 2026-04-27 16:13 UTC
Focus: `base-batcher-service` recent-tx startup scan staggered-ready multi-block benchmark coverage.
Hypothesis: the prior delayed-ready harness forced every channel to complete on the same final block, which still compressed all decode/drain work into one step; a staggered-ready fixture where channels complete across successive blocks should better expose whether touched-only draining keeps its advantage once readiness is distributed over time and whether tracker reuse matters more in that incremental-completion shape.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_ready_transition -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with `multi_block_staggered_ready_tx_payloads` and a new `process_blocks_staggered_ready` Criterion group
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_staggered_ready -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- re-ran the existing delayed-ready harness before editing the bench file to refresh the baseline shape on this machine: `baseline_vec_scan_all_4_blocks_1024_channels_ready_on_final_block` = `2.6685 ms .. 2.6910 ms`, `fresh_tracker_4_blocks_1024_channels_ready_on_final_block` = `2.3085 ms .. 2.3116 ms`, and `reused_tracker_4_blocks_1024_channels_ready_on_final_block` = `2.3507 ms .. 2.3932 ms`
- added benchmark-only staggered-ready coverage where `1024` channels are evenly distributed across four completion blocks (`25%` become ready on each block), keeping production logic unchanged
- the new staggered-ready harness showed the touched-only path still beats the old full-map scan when readiness is spread over time:
  - `baseline_vec_scan_all_4_blocks_1024_channels_ready_in_quarters`: `2.2951 ms .. 2.3111 ms`
  - `fresh_tracker_4_blocks_1024_channels_ready_in_quarters`: `2.1147 ms .. 2.1479 ms` (~7.4% lower median)
  - `reused_tracker_4_blocks_1024_channels_ready_in_quarters`: `2.1165 ms .. 2.1321 ms` (~7.7% lower median, effectively tied with fresh allocation and only ~0.3% lower median)
- focused validation passed again: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Next:
- if this startup-scan path is revisited again, extend the readiness matrix again (for example front-loaded, back-loaded, and mixed completion ratios) to map the crossover between incremental decode cost and touched-only drain savings before attempting any further production optimization

## 2026-04-27 18:19 UTC
Focus: `base-batcher-service` recent-tx startup scan weighted readiness benchmark coverage.
Hypothesis: the prior delayed/staggered multi-block fixtures showed touched-only draining still wins when readiness is distributed across blocks, but they did not distinguish whether the win is stronger when channels complete early versus late; adding front-loaded and back-loaded readiness matrices should make that crossover explicit before any further production optimization.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_staggered_ready -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with `multi_block_weighted_ready_tx_payloads`, front-loaded/back-loaded readiness distributions, and a new `process_blocks_weighted_ready` Criterion group
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_weighted_ready -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- refreshed the even staggered-ready baseline before editing the bench file: `baseline_vec_scan_all_4_blocks_1024_channels_ready_in_quarters` = `2.2775 ms .. 2.3121 ms`, `fresh_tracker_4_blocks_1024_channels_ready_in_quarters` = `2.1231 ms .. 2.1526 ms`, and `reused_tracker_4_blocks_1024_channels_ready_in_quarters` = `2.1196 ms .. 2.1639 ms`, confirming the touched-only path still holds an about `6.8%` win when completions are evenly distributed
- added benchmark-only weighted readiness coverage where `1024` channels complete across four blocks in front-loaded (`512/256/128/128`) and back-loaded (`128/128/256/512`) distributions, keeping production logic unchanged
- the new weighted harness shows touched-only draining helps in both shapes, with a larger gain when completion is back-loaded:
  - front-loaded: `baseline_vec_scan_all_front_loaded_4_blocks_1024_channels` = `2.1548 ms .. 2.1787 ms`, `fresh_tracker_front_loaded_4_blocks_1024_channels` = `2.0488 ms .. 2.0856 ms` (~`4.7%` lower median), `reused_tracker_front_loaded_4_blocks_1024_channels` = `2.0433 ms .. 2.0741 ms` (~`4.8%` lower median)
  - back-loaded: `baseline_vec_scan_all_back_loaded_4_blocks_1024_channels` = `2.4837 ms .. 2.5261 ms`, `fresh_tracker_back_loaded_4_blocks_1024_channels` = `2.2126 ms .. 2.2628 ms` (~`10.6%` lower median), `reused_tracker_back_loaded_4_blocks_1024_channels` = `2.2077 ms .. 2.2229 ms` (~`11.5%` lower median)
- this makes the crossover clearer: touched-only draining buys more once a larger survivor-heavy channel set persists into later blocks, while tracker reuse remains effectively tied with fresh allocation and at most a small secondary factor
- focused validation passed again: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Next:
- if this startup-scan path is revisited again, add a mixed readiness matrix with uneven per-block touch counts (not just completion counts) so the next experiment can separate savings from survivor-heavy draining versus savings from fewer per-block frame parses

## 2026-04-27 20:25 UTC
Focus: `base-batcher-service` recent-tx startup scan mixed readiness benchmark coverage with uneven touch-start timing.
Hypothesis: the weighted readiness matrix still starts every channel on block 0, so it conflates survivor-heavy drain savings with the cost of touching every channel on every earlier block; a cohort-based mixed fixture where some channels first appear on later blocks should separate those effects before any more production changes.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_weighted_ready -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with `ReadyTransitionCohort`, `multi_block_cohort_ready_tx_payloads`, and a new `process_blocks_mixed_ready` Criterion group
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_mixed_ready -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- refreshed the weighted-ready baseline before editing the bench file: front-loaded median `2.2425 ms` baseline vs `2.0634 ms` fresh tracker (~`8.0%` lower) vs `2.0278 ms` reused tracker (~`9.6%` lower); back-loaded median `2.5104 ms` baseline vs `2.2357 ms` fresh tracker (~`10.9%` lower) vs `2.1885 ms` reused tracker (~`12.8%` lower)
- added benchmark-only mixed readiness coverage where `1024` channels are split into five cohorts with staggered start blocks and back-loaded completion (`128` ready on block 0, `128` on block 1, `256` starting at block 1 and ready on block 2, `256` spanning all four blocks, and `256` starting at block 2 and ready on block 3)
- the new mixed fixture showed the touched-only path still wins when later cohorts do not exist in early blocks, but the gain is smaller than the fully back-loaded weighted case: `baseline_vec_scan_all_mixed_back_loaded_4_blocks_1024_channels` = `2.2023 ms`, `fresh_tracker_mixed_back_loaded_4_blocks_1024_channels` = `2.0648 ms` (~`6.2%` lower), `reused_tracker_mixed_back_loaded_4_blocks_1024_channels` = `2.0934 ms` (~`4.9%` lower)
- compared with the weighted back-loaded result, the reduced win in the mixed fixture suggests a meaningful part of the touched-only benefit comes from avoiding scans over long-lived survivor-heavy channels, not just from distributing completion later in time
- focused validation passed again: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Next:
- if this startup-scan path is revisited again, add another mixed fixture that holds completion timing constant while varying only touch-start sparsity, so the next experiment can quantify how much of the win comes from survivor-heavy buffered maps versus per-block frame parsing volume

## 2026-04-27 22:31 UTC
Focus: `base-batcher-service` recent-tx startup scan touch-start sparsity benchmark coverage.
Hypothesis: the new mixed readiness fixture suggested part of the touched-only drain win comes from survivor-heavy buffered channels, but it still did not compare that against a dense-start shape with similar back-loaded completion; adding a paired dense-start vs sparse-start benchmark should isolate how much of the win comes from later touch-start sparsity versus long-lived survivor-heavy channels.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_weighted_ready -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_mixed_ready -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with a new `process_blocks_touch_start_sparsity` Criterion group that runs paired dense-start and sparse-start back-loaded fixtures
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_touch_start_sparsity -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- refreshed the existing baselines before editing the bench file: weighted back-loaded median `2.4868 ms` baseline vs `2.2218 ms` fresh tracker (~`10.7%` lower) vs `2.1999 ms` reused tracker (~`11.5%` lower); mixed sparse-start median `2.2125 ms` baseline vs `2.0913 ms` fresh tracker (~`5.5%` lower) vs `2.0833 ms` reused tracker (~`5.8%` lower)
- added benchmark-only paired coverage that keeps the same approximate back-loaded completion shape (`128/128/256/512`) but compares a dense-start fixture where every channel begins on block 0 against the existing sparse-start cohort fixture where later channels only appear on later blocks
- the new pairwise comparison makes the survivor-heavy contribution explicit: dense-start baseline `2.4536 ms` vs sparse-start baseline `2.2503 ms` (~`8.3%` lower just from later touch-start sparsity), while the touched-only path still improves both shapes but wins more on dense-start survivor-heavy blocks: dense-start fresh tracker `2.2463 ms` (~`8.4%` lower) and reused tracker `2.2340 ms` (~`9.0%` lower) versus sparse-start fresh tracker `2.1074 ms` (~`6.4%` lower) and reused tracker `2.1106 ms` (~`6.2%` lower)
- this narrows the interpretation: a meaningful part of the startup-scan gain comes from avoiding scans over channels that have existed since early blocks, while later touch-start sparsity alone already removes a large slice of the old full-scan cost
Next:
- if this startup-scan path is revisited again, add a third pair where dense-start and sparse-start fixtures keep both completion counts and per-block transaction counts even closer so the remaining gap can be attributed almost entirely to survivor-heavy channel-map size rather than total parse volume

## 2026-04-28 00:36 UTC
Focus: `base-batcher-service` recent-tx startup scan matched-volume touch-start benchmark coverage.
Hypothesis: the prior dense-start vs sparse-start pair still changed per-block transaction volume, so some of the baseline gap could still come from less frame parsing; adding a matched-volume sparse-start fixture should isolate survivor-heavy buffered-map cost more cleanly by keeping block-by-block payload counts aligned with the dense-start back-loaded shape.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_touch_start_sparsity -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extended `crates/batcher/service/benches/recent_txs.rs` with `MATCHED_VOLUME_BACK_LOADED_READY_COHORTS` and a new `process_blocks_touch_start_matched_volume` Criterion group
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs process_blocks_touch_start_matched_volume -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- refreshed the existing touch-start sparsity baseline before editing the bench file: dense-start baseline `2.4297 ms`, fresh tracker `2.1906 ms`, reused tracker `2.2198 ms`; sparse-start baseline `2.2413 ms`, fresh tracker `2.1163 ms`, reused tracker `2.1221 ms`
- added benchmark-only matched-volume coverage where the sparse-start shape keeps the same total `1024` channels and the same per-block completion distribution as the dense-start back-loaded fixture, but delays only a `128`-channel cohort from block `0` to block `1`
- the matched-volume harness reduced the old dense-vs-sparse baseline gap to about `0.8%` (`2.4297 ms` dense vs `2.4097 ms` matched-volume sparse), showing most of the earlier `8%+` gap came from reduced parse volume rather than survivor-heavy map size alone
- even with matched block-by-block payload volume, the touched-only drain still beat the full scan in both shapes: dense-start fresh tracker `2.1906 ms` (~`9.8%` lower) and reused tracker `2.2198 ms` (~`8.6%` lower); matched-volume sparse-start fresh tracker `2.2298 ms` (~`7.5%` lower) and reused tracker `2.1861 ms` (~`9.3%` lower)
- this narrows the interpretation further: touched-only draining still has a durable win even after controlling for parse volume, but the extra dense-vs-sparse gap is much smaller once per-block transaction counts are matched, so future production changes should target the drain/decode path rather than assuming large additional gains from touch-start sparsity alone
- focused validation passed again: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Next:
- if this startup-scan path is revisited again, add a benchmark that holds both block payload counts and touched-ID cardinality constant while varying ready-channel decode density, so the next experiment can decide whether any remaining optimization opportunity sits in touched-only draining, channel decode, or `Frame::parse_frames`

## 2026-04-28 02:42 UTC
Focus: `base-batcher-service` recent-tx startup scan decode-density benchmark coverage with constant touched-ID cardinality.
Hypothesis: the matched-volume touch-start harness narrowed the survivor-heavy map effect, but it still left ambiguity about how much remaining variance comes from ready-channel decode versus drain bookkeeping; a prebuffered single-block fixture with fixed touched cardinality and payload count should isolate that crossover.
Commands:
- `cargo fmt --all`
- `cargo bench -p base-batcher-service --bench recent_txs process_block_decode_density -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
Results:
- added benchmark-only coverage in `crates/batcher/service/benches/recent_txs.rs` for `process_block_decode_density`, using a new `prebuffered_decode_density_fixture` that starts `1024` touched channels with two frames already buffered and then replays one current-block frame per channel while varying how many touched channels become ready on that block (`0`, `256`, `512`, `1024`)
- this fixture keeps touched-ID cardinality and current-block payload count constant, separating ready-channel decode cost from touched-only drain bookkeeping without changing production logic
- validation passed: focused tests still pass (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings` passed
- median benchmark results from Criterion `estimates.json`:
  - `0` ready channels: baseline full scan `284.11 µs` vs tracker+touched-only `107.05 µs` (~`62.3%` lower)
  - `256` ready channels: baseline `711.99 µs` vs tracker+touched-only `588.78 µs` (~`17.3%` lower)
  - `512` ready channels: baseline `1.2746 ms` vs tracker+touched-only `1.0348 ms` (~`18.8%` lower)
  - `1024` ready channels: baseline `2.2045 ms` vs tracker+touched-only `1.9483 ms` (~`11.6%` lower)
- the new harness makes the crossover clearer: touched-only draining remains beneficial across the whole range, but the relative win shrinks as more touched channels finish and decode cost dominates more of the block budget
Next:
- if this startup-scan path is revisited again, use the new decode-density harness to test any drain/decode candidate directly; the next likely production opportunity is in channel decode or `Frame::parse_frames`, not in another touched-ID bookkeeping tweak

## 2026-04-28 04:48 UTC
Focus: `base-batcher-service` recent-tx startup scan decode path in `RecentTxScanner::decode_channel()`.
Hypothesis: the decode path still clones concatenated channel frame data with `data.to_vec()` before constructing `BatchReader`; passing the owned `Bytes` directly should avoid one full payload copy per ready channel while preserving decoding semantics.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_block_decode_density -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- edited `crates/batcher/service/src/recent_txs.rs` to pass `channel.frame_data()` directly into `BatchReader::new`
- `cargo fmt --all`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-batcher-service --bench recent_txs process_block_decode_density -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extracted median `point_estimate` values from `target/criterion/.../estimates.json` for before/after comparison
Results:
- changed `RecentTxScanner::decode_channel()` from `BatchReader::new(data.to_vec(), max_rlp)` to `BatchReader::new(data, max_rlp)`, removing an unnecessary `Bytes`→`Vec<u8>` clone before decompression while leaving all tests green
- validation passed: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
- decode-density median results after the change (before → after):
  - `0` ready channels: baseline full scan `284.11 µs` → `197.00 µs`; tracker+touched-only `107.05 µs` → `105.80 µs`
  - `256` ready channels: baseline `711.99 µs` → `686.94 µs`; tracker+touched-only `588.78 µs` → `572.95 µs`
  - `512` ready channels: baseline `1.2746 ms` → `1.1505 ms`; tracker+touched-only `1.0348 ms` → `1.0337 ms`
  - `1024` ready channels: baseline `2.2045 ms` → `2.1306 ms`; tracker+touched-only `1.9483 ms` → `1.9690 ms`
- the signal is modest and somewhat noisy, but the touched-only path stayed essentially flat or slightly better on three of four decode-density cases while the code simplification safely removes one per-channel buffer materialization from the hot decode path
Next:
- if this startup-scan path is revisited again, add a decode-only microbenchmark around `channel.frame_data()` + `BatchReader::next_batch()` so future work can isolate frame concatenation and decompression costs from touched-ID tracking and channel-map drain behavior.

## 2026-04-28 06:57 UTC
Focus: `base-batcher-service` recent-tx startup scan decode-component benchmark coverage.
Hypothesis: the decode-density harness still mixed touched-ID draining, frame parsing, `channel.frame_data()`, and `BatchReader` work; a decode-only component bench should isolate whether the next likely hotspot is frame concatenation or decompression/decoding before touching production code again.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs process_block_decode_density -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- edited `crates/batcher/service/benches/recent_txs.rs` to add `decode_channel_components` coverage with ready multi-batch channels split across `1`, `4`, and `16` frames
- `cargo fmt --all`
- `cargo test -p base-batcher-service recent_txs -- --nocapture`
- `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-batcher-service --bench recent_txs decode_channel_components -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- extracted median `point_estimate` values from `target/criterion/.../estimates.json`
Results:
- kept production logic unchanged and added benchmark-only helpers that separately measure `channel.frame_data()` alone, `BatchReader` on preaggregated channel bytes alone, and the combined decode path for a `16`-batch ready channel
- validation passed: `cargo test -p base-batcher-service recent_txs -- --nocapture` (`8 passed`) and `cargo clippy -p base-batcher-service --tests --benches --no-deps -- -D warnings`
- refreshed decode-density medians on the current tree to preserve continuity before the harness edit:
  - `0` ready channels: baseline full scan `205.30 µs`; tracker+touched-only `104.18 µs` (~`49.3%` lower)
  - `256` ready channels: baseline `681.31 µs`; tracker+touched-only `589.41 µs` (~`13.5%` lower)
  - `512` ready channels: baseline `1.1581 ms`; tracker+touched-only `1.0310 ms` (~`11.0%` lower)
  - `1024` ready channels: baseline `2.1614 ms`; tracker+touched-only `1.9537 ms` (~`9.6%` lower)
- new decode-component medians show `BatchReader` dominates while `channel.frame_data()` stays a small but growing additive cost as frame count rises:
  - `1` frame: `frame_data_only` `19.52 ns`; `batch_reader_only` `3.4034 µs`; combined `3.4197 µs`
  - `4` frames: `frame_data_only` `25.62 ns`; `batch_reader_only` `3.4136 µs`; combined `3.3986 µs`
  - `16` frames: `frame_data_only` `82.71 ns`; `batch_reader_only` `3.4251 µs`; combined `3.4930 µs`
- interpretation: even when a ready channel is split across many frames, frame concatenation remains tiny relative to decompression and batch decoding, so the next production opportunity is more likely inside `BatchReader`/decode work than in additional `channel.frame_data()` tweaks
Next:
- if this path is revisited again, inspect `BatchReader::decompress` and per-batch decode overhead for reusable scratch buffers or avoidable allocations, using the new component harness to verify whether any candidate actually moves the `~3.4 µs` decode floor.

## 2026-04-28 09:15 UTC
Focus: `base-protocol::BatchReader` raw-input ownership on the shared decode path used by batcher and consensus derivation.
Hypothesis: `BatchReader::new` still stores raw channel bytes as `Vec<u8>`, so callers that already own `Bytes` pay an avoidable clone before decompression; switching `BatchReader` to hold `Bytes` directly and adding a side-by-side benchmark should eliminate that constructor copy and show whether decode throughput improves on larger multi-batch channels.
Commands:
- `cargo bench -p base-batcher-service --bench recent_txs decode_channel_components -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 15`
- edited `crates/consensus/protocol/src/batch/reader.rs` to store raw input as `Option<Bytes>` and accept `Into<Bytes>` in `BatchReader::new`
- edited `crates/consensus/derive/src/stages/channel/channel_reader.rs` to pass owned channel bytes into `BatchReader::new`
- added `crates/consensus/protocol/benches/batch_reader.rs` plus Criterion wiring in `crates/consensus/protocol/Cargo.toml`
- `cargo bench -p base-protocol --bench batch_reader -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 20`
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo test -p base-consensus-derive channel_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo clippy -p base-consensus-derive --tests --no-deps -- -D warnings`
- extracted median `point_estimate` values from `target/criterion/protocol_batch_reader_*/*/*/base/estimates.json`
Results:
- changed `BatchReader` to retain owned raw input as `Bytes` instead of `Vec<u8>`, letting callers such as `ChannelReader` move channel payloads into the reader without an intermediate heap clone
- added a new shared protocol benchmark that measures both the old `Bytes::to_vec()` constructor path and the new owned-`Bytes` path for constructor-only cost and full decode cost, using `1`-batch and `64`-batch synthetic channels derived from the existing `batch.hex` fixture
- validation passed: focused protocol tests (`2 passed`), focused derive tests (`7 passed`), `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`, and `cargo clippy -p base-consensus-derive --tests --no-deps -- -D warnings`
- constructor benchmark medians show the copy elimination clearly:
  - `1` batch: baseline `Bytes::to_vec()` `8.045 µs` vs owned `Bytes` `5.96 ns`
  - `64` batches: baseline `Bytes::to_vec()` `9.113 ms` vs owned `Bytes` `5.93 ns`
- full decode benchmark medians show the copy matters most once the channel is larger:
  - `1` batch: baseline `5.452 ms` vs owned `Bytes` `5.554 ms` (effectively noise / no durable win at this size)
  - `64` batches: baseline `382.56 ms` vs owned `Bytes` `369.54 ms` (~`3.4%` lower median)
- interpretation: the removed input clone is a large constructor-only win and a modest but measurable end-to-end win on larger multi-batch channels, which is consistent with decompression and batch decoding still dominating small ready-channel work
Next:
- if this shared decode path is revisited again, extend the new `base-protocol` bench with a decompression-only split (zlib vs brotli) so the next iteration can tell whether any remaining decode floor is in `decompress_*` itself or in per-batch RLP decoding after decompression.

## 2026-04-28 11:24 UTC
Focus: `base-protocol` shared decode-path decompression-only benchmark coverage.
Hypothesis: the new shared `BatchReader` bench still mixes codec work with per-batch decoding, so adding a decompression-only split for zlib and brotli on the same synthetic fixtures should show whether the remaining `64`-batch decode floor is mostly in decompression or in post-decompression batch decoding.
Commands:
- `cargo bench -p base-protocol --bench batch_reader decode_all_batches -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 20`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add a reusable decompressed fixture builder plus a new `protocol/batch_reader/decompression_only` Criterion group covering zlib and brotli at `1` and `64` batches
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader decompression_only -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 20`
- `cargo bench -p base-protocol --bench batch_reader decode_all_batches -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 20`
- extracted median `point_estimate` values from `target/criterion/protocol_batch_reader_*/*/*/base/estimates.json`
Results:
- kept production logic unchanged and extended the shared protocol bench to measure decompression in isolation, which closes the remaining observability gap from the prior run without changing consensus or batcher behavior
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- constructor medians remain the same order of magnitude as the prior run, confirming the owned-`Bytes` copy removal still dominates constructor-only cost: `1` batch baseline `8.045 µs` vs owned `5.96 ns`; `64` batches baseline `9.113 ms` vs owned `5.93 ns`
- new decompression-only medians show codec cost already dominates the decode floor, and brotli is materially faster than zlib on the shared synthetic fixture sizes:
  - `1` batch: zlib `1.960 ms`, brotli `4.208 ms`
  - `64` batches: zlib `143.22 ms`, brotli `70.74 ms`
- refreshed full decode medians after the bench expansion:
  - `1` batch: baseline `5.356 ms` vs owned `5.368 ms` (effectively noise)
  - `64` batches: baseline `375.47 ms` vs owned `363.63 ms` (~`3.2%` lower median)
- interpretation: for large channels, decompression accounts for a substantial share of total decode time (`~38%` for zlib and `~19%` for brotli relative to the current `64`-batch full-decode medians), so the next meaningful optimization likely sits in zlib decompression or in the remaining per-batch RLP decode work rather than in additional constructor plumbing
Next:
- if this shared decode path is revisited again, split the `decode_all_batches` harness one step further by compression type so the next iteration can quantify how much of the remaining `~220 ms` to `~293 ms` non-constructor cost is specific to zlib channels versus shared post-decompression batch decoding.

## 2026-04-28 13:39 UTC
Focus: `base-protocol` shared decode-path benchmark correctness for compression-specific `BatchReader` decoding.
Hypothesis: the current `decode_all_batches` bench only exercises zlib-valid fixtures/config, so adding protocol-valid brotli fixtures and a Fjord-active config should expose whether the earlier shared decode numbers were accidentally measuring early returns instead of real brotli decode work.
Commands:
- `cargo bench -p base-protocol --bench batch_reader decode_all_batches -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 20`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add compression-tagged fixtures, prepend the brotli channel-version byte, and select a Fjord-active `RollupConfig` for brotli decode cases
- `cargo fmt --all`
- `cargo bench -p base-protocol --bench batch_reader decode_all_batches -- --warm-up-time 0.5 --measurement-time 1.0 --sample-size 20`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- extracted median `point_estimate` values from `target/criterion/protocol_batch_reader_decode_all_batches/*/*/base/estimates.json`
Results:
- corrected the shared decode benchmark so brotli cases now use protocol-valid channel bytes (`CHANNEL_VERSION_BROTLI` prefix) and a Fjord-active rollup config instead of accidentally short-circuiting on unsupported-type or pre-Fjord checks
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- refreshed decode medians now split cleanly by compression type:
  - zlib `1` batch: baseline `5.3706 ms` vs owned `5.4123 ms` (~`0.8%` slower, noise)
  - brotli `1` batch: baseline `7.9290 ms` vs owned `8.0884 ms` (~`2.0%` slower, noise)
  - zlib `64` batches: baseline `376.38 ms` vs owned `361.43 ms` (~`4.0%` lower median)
  - brotli `64` batches: baseline `290.80 ms` vs owned `288.24 ms` (~`0.9%` lower median)
- interpretation: the earlier sub-microsecond brotli results were benchmark-fixture artifacts, not real decode throughput; with valid brotli inputs the shared decode floor is on the same order as zlib and still shows the strongest owned-`Bytes` win on large zlib channels
Next:
- if this shared decode path is revisited again, extend the protocol bench with separate post-decompression decode coverage (for example feed pre-decompressed zlib and brotli payloads into the same batch-decoding loop) so the next iteration can quantify how much of the remaining large-channel gap is codec-specific versus shared RLP/batch decoding.

## 2026-04-28 16:01 UTC
Focus: `base-protocol` shared decode-path split between decompression and post-decompression batch decoding.
Hypothesis: after correcting protocol-valid brotli fixtures, a post-decompression decode-only harness should show whether the remaining large-channel floor is still mostly codec work or mostly shared RLP/batch decoding.
Commands:
- `cargo bench -p base-protocol --bench batch_reader decode_all_batches -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add `decode_all_batches_from_decompressed` and a new `protocol/batch_reader/post_decompression_decode_only` Criterion group
- fixed the existing brotli decompression-only bench to strip the leading `CHANNEL_VERSION_BROTLI` byte before calling the raw `Brotli.decompress` helper
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader post_decompression_decode_only -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo bench -p base-protocol --bench batch_reader decompression_only -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo bench -p base-protocol --bench batch_reader decode_all_batches -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extracted median `point_estimate` values from `target/criterion/protocol_batch_reader_{post_decompression_decode_only,decompression_only,decode_all_batches}/*/*/new/estimates.json`
Results:
- added benchmark-only coverage that decodes batches from already-decompressed channel payloads, isolating shared `Bytes::decode` + `Batch::decode` work from the codec step without changing production logic
- corrected the raw brotli decompression microbench so it now measures the actual compressed payload rather than the protocol wrapper byte; the old helper expects the payload after the version byte even though `BatchReader` itself consumes the tagged channel bytes
- validation passed again: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- median benchmark results on the current tree:
  - post-decompression decode-only: zlib `1` batch `3.338 ms`, brotli `1` batch `3.378 ms`, zlib `64` batches `224.88 ms`, brotli `64` batches `223.63 ms`
  - decompression-only: zlib `1` batch `1.928 ms`, brotli `1` batch `4.078 ms`, zlib `64` batches `134.25 ms`, brotli `64` batches `59.66 ms`
  - full decode with owned `Bytes`: zlib `1` batch `5.328 ms`, brotli `1` batch `7.693 ms`, zlib `64` batches `353.93 ms`, brotli `64` batches `278.80 ms`
  - refreshed full-decode baseline vs owned comparison on the same run: zlib `64` batches `365.03 ms` baseline vs `353.93 ms` owned (~`3.0%` lower); brotli `64` batches `278.34 ms` baseline vs `278.80 ms` owned (effectively flat / noise)
- interpretation: on large zlib channels the remaining floor is split between codec work (`134.25 ms`, ~`37.9%` of total) and post-decompression batch decode (`224.88 ms`, ~`63.5%` of total), while on large brotli channels most of the cost is now clearly in shared post-decompression decode (`223.63 ms`, ~`80.2%` of total) rather than brotli itself (`59.66 ms`, ~`21.4%`)
Next:
- if this decode path is revisited again, profile or microbenchmark the shared post-decompression path itself (for example `Bytes::decode` framing versus `Batch::decode`/span derivation) because that now looks like the dominant remaining hotspot, especially for brotli channels.

## 2026-04-28 18:08 UTC
Focus: `base-protocol` shared post-decompression decode split between outer RLP framing and `Batch::decode` work.
Hypothesis: the prior post-decompression harness still mixed `Bytes::decode` framing with actual batch decoding, so splitting those stages should reveal whether the remaining floor is in outer RLP payload extraction or inside batch-specific decode/derive logic.
Commands:
- `cargo bench -p base-protocol --bench batch_reader post_decompression_decode_only -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add `batch_payloads_from_decompressed`, `count_rlp_wrapped_batches`, `decode_all_batch_payloads`, and a new `protocol/batch_reader/post_decompression_components` Criterion group
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader post_decompression_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extracted median `point_estimate` values from `target/criterion/protocol_batch_reader_{post_decompression_decode_only,post_decompression_components}/*/*/new/estimates.json`
Results:
- kept production logic unchanged and added benchmark-only coverage that splits the shared post-decompression path into outer `Bytes::decode` framing (`rlp_only_*`) versus `Batch::decode` on pre-extracted payloads (`batch_decode_only_*`)
- validation passed again: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- refreshed post-decompression decode-only medians: zlib `1` batch `3.360 ms`, brotli `1` batch `3.416 ms`, zlib `64` batches `225.80 ms`, brotli `64` batches `224.80 ms`
- new component medians:
  - outer RLP framing only: zlib `1` batch `27.57 µs`, brotli `1` batch `25.93 µs`, zlib `64` batches `5.844 ms`, brotli `64` batches `5.441 ms`
  - `Batch::decode` on pre-extracted payloads: zlib `1` batch `3.384 ms`, brotli `1` batch `3.398 ms`, zlib `64` batches `221.89 ms`, brotli `64` batches `221.59 ms`
- interpretation: outer RLP framing is only about `0.8%` of the single-batch post-decompression path and about `2.4%` to `2.6%` of the `64`-batch path; nearly all remaining shared decode cost sits inside `Batch::decode`/span derivation rather than payload extraction
Next:
- if this decode path is revisited again, add a deeper component split inside `Batch::decode` itself (for example `SingleBatch::decode` versus `RawSpanBatch::decode` plus span derivation) so the next experiment can identify whether large-channel cost is dominated by raw span parsing, transaction-data decoding, or span derivation.

## 2026-04-28 20:10 UTC
Focus: `base-protocol` shared post-decompression span decode split inside `Batch::decode`.
Hypothesis: after isolating outer RLP framing, the remaining floor likely sits inside span-batch decoding; a deeper span-specific benchmark should reveal whether large-channel cost is dominated by raw span parsing, transaction reconstruction in `SpanBatchTransactions::full_txs`, or final `RawSpanBatch::derive` work.
Commands:
- `cargo bench -p base-protocol --bench batch_reader post_decompression_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add `span_batch_payloads_from_decompressed`, `raw_span_batch_templates_from_decompressed`, `decode_all_raw_span_batches`, `decode_all_raw_span_full_txs`, `derive_all_raw_span_batches`, and a new `protocol/batch_reader/batch_decode_components` Criterion group
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extracted median `point_estimate` values from `target/criterion/protocol_batch_reader_{post_decompression_components,batch_decode_components}/*/*/new/estimates.json`
Results:
- kept production logic unchanged and extended the shared protocol benchmark to strip the leading span batch-type byte from each decompressed RLP payload, then measure three deeper `Batch::decode` stages independently: `RawSpanBatch::decode`, `SpanBatchTransactions::full_txs` on predecoded templates, and full `RawSpanBatch::derive`
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- refreshed zlib post-decompression `Batch::decode` medians from the existing harness: `1` batch `3.3849866 ms`, `64` batches `222.0288915 ms`
- new span-component medians:
  - `RawSpanBatch::decode` only: `1` batch `192.06 µs`, `64` batches `17.576 ms`
  - `SpanBatchTransactions::full_txs` only: `1` batch `3.0213 ms`, `64` batches `211.24 ms`
  - full `RawSpanBatch::derive`: `1` batch `3.1785 ms`, `64` batches `221.62 ms`
- interpretation: raw span parsing is only about `5.7%` of the single-batch zlib post-decompression floor and `7.9%` of the `64`-batch floor, while `SpanBatchTransactions::full_txs` accounts for about `89%` of the single-batch cost and about `95%` of the `64`-batch cost; the remaining derive step above `full_txs` is comparatively small, so future optimization work should focus on transaction reconstruction/encoding inside `full_txs` rather than on raw span parsing or prefix/origin derivation
Next:
- if this decode path is revisited again, drill into `SpanBatchTransactions::full_txs` itself (for example `SpanBatchTransactionData::decode`, signature/to-field assembly, and `TxEnvelope::encode_2718`) so the next experiment can identify whether the remaining shared decode floor is dominated by span transaction-data parsing or transaction re-encoding.

## 2026-04-28 22:23 UTC
Focus: `base-protocol` span transaction reconstruction allocation in `SpanBatchTransactions::full_txs()`.
Hypothesis: the span decode component bench now shows `SpanBatchTransactions::full_txs()` dominates `Batch::decode`, and the method already knows the exact number of output transactions up front; preallocating the output `Vec<Vec<u8>>` to `total_block_tx_count` should remove repeated outer-vector growth during reconstruction and yield a small measurable win without changing semantics.
Commands:
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/src/batch/transactions.rs` to change `full_txs()` from `Vec::new()` to `Vec::with_capacity(self.total_block_tx_count as usize)`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
Results:
- made a one-line production change that preallocates the outer `txs` vector in `SpanBatchTransactions::full_txs()`, matching the already-known transaction count and avoiding incremental growth while reconstructing encoded transactions
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- focused benchmark results from the before/after `batch_decode_components` runs showed a small but measurable improvement in the hot `full_txs` stage while adjacent stages stayed effectively flat:
  - `span_full_txs_only/1`: `3.0463 ms` -> `3.0158 ms` (~`1.0%` lower median)
  - `span_full_txs_only/64`: `214.02 ms` -> `213.03 ms` (~`0.5%` lower median)
  - `raw_span_decode_only/64`: `18.330 ms` -> `17.719 ms` (same order of magnitude; not the target of the change)
  - `span_derive_only/64`: `223.09 ms` -> `222.24 ms` (effectively flat / within noise)
- interpretation: most of the remaining `full_txs` floor is still inside transaction-data decode and transaction re-encoding, but preallocating the outer result buffer is a safe low-risk cleanup that trims a little allocator churn from the hottest measured substage
Next:
- if this decode path is revisited again, microbenchmark `SpanBatchTransactionData::decode` against `TxEnvelope::encode_2718` inside `full_txs()` so the next iteration can tell whether the remaining shared decode floor is dominated by span transaction-data parsing or by re-encoding reconstructed signed transactions.

## 2026-04-29 00:33 UTC
Focus: `base-protocol` span transaction re-encoding allocation in `SpanBatchTransactions::full_txs()`.
Hypothesis: the remaining `full_txs()` floor is likely split across transaction-data decode, signed-envelope construction, and `encode_2718`; if the encoding stage is still allocating from `Vec::new()` despite knowing `encode_2718_len()`, preallocating each transaction buffer should produce a measurable win and a new component harness should show how much of `full_txs()` it removes.
Commands:
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extended `crates/consensus/protocol/benches/batch_reader.rs` with a new `protocol/batch_reader/span_full_txs_components` Criterion group that splits `SpanBatchTransactions::full_txs()` into `SpanBatchTransactionData::decode`, `SpanBatchTransactionData::to_signed_tx`, and `TxEnvelope::encode_2718`, including both `Vec::new()` and `Vec::with_capacity(tx_envelope.encode_2718_len())` encoding cases
- edited `crates/consensus/protocol/src/batch/transactions.rs` to change the per-transaction encode buffer from `Vec::new()` to `Vec::with_capacity(tx_envelope.encode_2718_len())`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo bench -p base-protocol --bench batch_reader span_full_txs_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
Results:
- added durable benchmark coverage that makes the hottest remaining `full_txs()` stages explicit instead of guessing from aggregate decode time alone
- the new component harness shows per-transaction signed-envelope construction dominates, but encoding allocation was still a meaningful secondary cost and exact-capacity buffers help a lot in isolation:
  - `span_tx_data_decode_only/1`: `101.33 µs`
  - `span_to_signed_tx_only/1`: `2.411 ms`
  - `span_encode_2718_only/1`: `302.79 µs`
  - `span_encode_2718_exact_capacity_only/1`: `145.36 µs` (~`52.0%` lower than `Vec::new()`)
  - `span_tx_data_decode_only/64`: `11.166 ms`
  - `span_to_signed_tx_only/64`: `160.47 ms`
  - `span_encode_2718_only/64`: `25.780 ms`
  - `span_encode_2718_exact_capacity_only/64`: `15.957 ms` (~`38.1%` lower than `Vec::new()`)
- the production change moved the full `full_txs()` stage by a meaningful amount once measured end-to-end in the existing harness:
  - `span_full_txs_only/1`: `3.0887 ms` -> `2.8193 ms` (~`8.7%` lower median from Criterion output)
  - `span_full_txs_only/64`: `214.58 ms` -> `196.27 ms` (~`8.5%` lower median)
  - `span_derive_only/64`: `223.39 ms` -> `205.48 ms` (~`8.0%` lower median), consistent with `derive` spending most of its time inside `full_txs()`
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
Next:
- if this decode path is revisited again, use the new `span_full_txs_components` harness to test whether `SpanBatchTransactionData::to_signed_tx` can avoid more temporary allocation or repeated setup, since it now accounts for the largest remaining share of the `full_txs()` floor.

## 2026-04-29 02:40 UTC
Focus: `base-protocol` span transaction reconstruction inside `SpanBatchTransactionData::to_signed_tx()`.
Hypothesis: the `to_signed_tx()` hot path still converts fee fields from `U256` to `u128` by materializing 32-byte big-endian arrays and slicing the low 16 bytes; replacing those conversions with direct `u128::try_from(&U256)` calls might remove a little per-transaction copying in the dominant reconstruction stage.
Commands:
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo bench -p base-protocol --bench batch_reader span_full_txs_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/src/batch/tx_data/{legacy,eip2930,eip1559,eip7702}.rs` to replace manual `to_be_bytes::<32>()[16..]` conversions with `u128::try_from(&U256)`
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo bench -p base-protocol --bench batch_reader span_full_txs_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- reverted the production edits after the confirming run showed no improvement on the targeted end-to-end stage
Results:
- the before-change refresh on the current tree kept the prior hot-stage picture intact: `span_full_txs_only/64` stayed at `196.57 ms` median and `span_to_signed_tx_only/64` at `160.15 ms`, with `span_encode_2718_only/64` at `25.072 ms`
- the attempted direct-conversion change did not improve the targeted reconstruction stage and slightly regressed the isolated component benches on this machine, so it was intentionally reverted and the branch is left clean
- confirming after-change medians before revert:
  - `span_full_txs_only/1`: `2.8439 ms` -> `2.8437 ms` (flat)
  - `span_full_txs_only/64`: `196.57 ms` -> `196.61 ms` (flat)
  - `span_to_signed_tx_only/1`: `2.3974 ms` -> `2.4117 ms` (~`0.6%` slower)
  - `span_to_signed_tx_only/64`: `160.15 ms` -> `161.22 ms` (~`0.7%` slower)
  - `span_encode_2718_exact_capacity_only/64`: `16.099 ms` -> `16.151 ms` (flat)
- validation on the temporary experiment still passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- PR status check: existing open PR remains `#2409` (`perf: optimize batcher hot paths`), so no new PR was opened
Next:
- revisit `SpanBatchTransactionData::to_signed_tx()` with a more structural candidate, likely reducing repeated cloning/setup inside typed transaction construction (`input`, access lists, or authorization lists) and validating it first against the existing `span_full_txs_components` harness before touching production code again.

## 2026-04-29 04:48 UTC
Focus: `base-protocol` span transaction reconstruction clone pressure inside `SpanBatchTransactions::full_txs()`.
Hypothesis: `full_txs()` decodes each `SpanBatchTransactionData` and then rebuilds a signed envelope from borrowed transaction data, so adding owned `into_signed_tx` variants and moving `Bytes`/access lists/authorization lists into typed transactions might cut clone overhead inside the dominant `span_to_signed_tx_only` stage.
Commands:
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo bench -p base-protocol --bench batch_reader span_full_txs_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/src/batch/tx_data/{legacy,eip2930,eip1559,eip7702,wrapper}.rs` plus `crates/consensus/protocol/src/batch/transactions.rs` to add owned `into_signed_tx` helpers and route `full_txs()` through them
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader batch_decode_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo bench -p base-protocol --bench batch_reader span_full_txs_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- reverted the production edits after the confirming benchmarks regressed or stayed within noise on the targeted hot path
Results:
- refreshed pre-change medians on the current tree: `span_full_txs_only/1` = `2.8310 ms`, `span_full_txs_only/64` = `196.16 ms`, `span_to_signed_tx_only/1` = `2.4103 ms`, `span_to_signed_tx_only/64` = `160.40 ms`, `span_tx_data_decode_only/64` = `11.226 ms`, and `span_encode_2718_exact_capacity_only/64` = `15.981 ms`
- the owned-conversion experiment validated cleanly but did not produce an end-to-end win and appears to hurt the isolated reconstruction component on this machine, so it was reverted and the branch is clean
- confirming after-change medians before revert:
  - `span_full_txs_only/1`: `2.8310 ms` -> `2.8514 ms` (~`0.7%` slower)
  - `span_full_txs_only/64`: `196.16 ms` -> `196.61 ms` (~`0.2%` slower / effectively flat)
  - `span_to_signed_tx_only/1`: `2.4103 ms` -> `2.4409 ms` (~`1.3%` slower)
  - `span_to_signed_tx_only/64`: `160.40 ms` -> `162.24 ms` (~`1.1%` slower)
  - `span_tx_data_decode_only/64`: `11.226 ms` -> `11.806 ms` (~`5.2%` slower)
  - `span_encode_2718_exact_capacity_only/64`: `15.981 ms` -> `16.029 ms` (flat)
- validation on the temporary experiment still passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- branch/PR hygiene check: `git status` is clean after revert and the existing open PR is still `#2409`, so there was nothing to commit or push this run
Next:
- avoid more ownership-plumbing experiments in `full_txs()` until a deeper component harness proves where the remaining `span_to_signed_tx_only` time is really spent; the next best target is a finer split inside signed transaction construction (for example signature-hash generation versus typed-transaction assembly per tx kind) so future edits can attack the dominant substage directly.

## 2026-04-29 06:57 UTC
Focus: `base-protocol` span signed-transaction construction decomposition inside `SpanBatchTransactionData::to_signed_tx()`.
Hypothesis: the remaining `span_to_signed_tx_only` floor may not be in transaction field assembly at all; a deeper benchmark that separates typed-transaction construction from `signature_hash()` generation should show whether another production edit is justified or whether hashing already dominates the stage.
Commands:
- `cargo bench -p base-protocol --bench batch_reader span_full_txs_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add a new `protocol/batch_reader/span_signed_tx_components` Criterion group with `span_build_typed_tx_only` and `span_signature_hash_only` fixtures alongside the existing `span_to_signed_tx_only` case
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader span_signed_tx_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extracted medians from `target/criterion/protocol_batch_reader_span_signed_tx_components/*/*/new/estimates.json`
- `gh pr list --head automation/perf-autopilot --state open`
Results:
- kept production logic unchanged and added benchmark-only coverage that rebuilds typed transactions from decoded span fixtures, then measures `signature_hash()` separately from typed-transaction field assembly
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- new median benchmark results show `signature_hash()` is overwhelmingly dominant inside `to_signed_tx()`:
  - `span_to_signed_tx_only/1`: `2.3998 ms`
  - `span_build_typed_tx_only/1`: `39.285 µs` (~`1.64%` of total)
  - `span_signature_hash_only/1`: `2.3287 ms` (~`97.0%` of total)
  - `span_to_signed_tx_only/64`: `160.14 ms`
  - `span_build_typed_tx_only/64`: `2.6204 ms` (~`1.64%` of total)
  - `span_signature_hash_only/64`: `154.78 ms` (~`96.6%` of total)
- interpretation: another ownership/plumbing change in typed transaction assembly is unlikely to move the end-to-end `full_txs()` floor much; nearly all of the remaining cost in `to_signed_tx()` is signature-hash generation
- branch/PR hygiene check: existing open PR remains `#2409` (`perf: optimize batcher hot paths`), so no new PR was opened
Next:
- if this path is revisited again, target `signature_hash()` specifically (or prove it cannot be amortized/cached safely in the span decode path) before attempting any more `to_signed_tx()` field-assembly changes.

## 2026-04-29 09:06 UTC
Focus: `base-protocol` span signature-hash cost distribution by transaction type.
Hypothesis: after confirming `signature_hash()` dominates `SpanBatchTransactionData::to_signed_tx()`, the next useful narrowing step is to measure hash cost by typed transaction kind; if one kind is materially more expensive per transaction, future hash-focused work should target that path first instead of treating all span transactions the same.
Commands:
- `cargo bench -p base-protocol --bench batch_reader span_signed_tx_components -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add `TypedTransactionKind`, grouping helpers, and a new `protocol/batch_reader/span_signature_hash_by_tx_type` Criterion group
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader span_signature_hash_by_tx_type -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extracted medians from `target/criterion/protocol_batch_reader_span_signature_hash_by_tx_type/*/*/new/estimates.json`
- `gh pr list --head automation/perf-autopilot --state open`
Results:
- kept production logic unchanged and added benchmark-only coverage that groups the same decoded span fixtures by typed transaction kind before measuring `signature_hash()`
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- the current fixture mix is almost entirely legacy and EIP-1559 transactions, with no EIP-7702 transactions present in `batch.hex`; the new bench therefore measured three active kinds on this machine
- median signature-hash timings from the new group:
  - `legacy_454_txs/1`: `983.158 µs` total, about `2.17 µs/tx`
  - `eip2930_11_txs/1`: `21.489 µs` total, about `1.95 µs/tx`
  - `eip1559_1301_txs/1`: `1.314 ms` total, about `1.01 µs/tx`
  - `legacy_29056_txs/64`: `66.855 ms` total, about `2.30 µs/tx`
  - `eip2930_704_txs/64`: `1.394 ms` total, about `1.98 µs/tx`
  - `eip1559_83264_txs/64`: `88.996 ms` total, about `1.07 µs/tx`
- interpretation: signature hashing is still the dominant stage overall, but its per-transaction cost is not uniform; legacy and EIP-2930 hashing are roughly 2x the per-tx cost of EIP-1559 on the current fixture mix, so any future optimization or upstream investigation should start with those hash paths or with fixture coverage that includes EIP-7702 before touching production decode logic
- branch/PR hygiene check: existing open PR remains `#2409` (`perf: optimize batcher hot paths`), so no new PR was opened
Next:
- if this decode path is revisited again, either add a fixture with meaningful EIP-7702 coverage or inspect whether legacy/EIP-2930 `signature_hash()` paths can be optimized or replaced by a cheaper safe construction in the span decode flow before attempting any production changes.

## 2026-04-29 11:20 UTC
Focus: `base-protocol` signature-hash benchmark coverage for EIP-7702 and tx-type apples-to-apples comparisons.
Hypothesis: the existing `span_signature_hash_by_tx_type` fixture is useful for real mix analysis but cannot measure EIP-7702 because `batch.hex` contains none; adding a synthetic same-cardinality tx-type benchmark should close that coverage gap and reveal whether the earlier per-tx ordering was mostly fixture-shape dependent.
Commands:
- `cargo bench -p base-protocol --bench batch_reader span_signature_hash_by_tx_type -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add synthetic typed-transaction fixtures plus a new `protocol/batch_reader/synthetic_signature_hash_by_tx_type` Criterion group
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader synthetic_signature_hash_by_tx_type -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extracted medians from `target/criterion/protocol_batch_reader_synthetic_signature_hash_by_tx_type/*/*/new/estimates.json`
- `gh pr list --head automation/perf-autopilot --state open`
Results:
- refreshed the existing fixture-backed hash benchmark before editing the bench file; it still measured only legacy, EIP-2930, and EIP-1559 because `batch.hex` contains no EIP-7702 transactions, confirming the coverage gap from the prior run
- added benchmark-only synthetic coverage that builds `1024` typed transactions per kind (`legacy`, `eip2930`, `eip1559`, `eip7702`) with the same cardinality and simple uniform payload shape, leaving production logic unchanged
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- median synthetic signature-hash timings from Criterion `estimates.json`:
  - `legacy_1024_txs/synthetic`: `273.52 µs` total, about `267 ns/tx`
  - `eip2930_1024_txs/synthetic`: `272.97 µs` total, about `267 ns/tx`
  - `eip1559_1024_txs/synthetic`: `307.85 µs` total, about `301 ns/tx`
  - `eip7702_1024_txs/synthetic`: `525.64 µs` total, about `513 ns/tx`
- interpretation: the earlier fixture-backed ranking was heavily influenced by the real span fixture mix and transaction shapes; under matched synthetic cardinality, EIP-7702 hashing is the most expensive of the four measured kinds while legacy and EIP-2930 are effectively tied and EIP-1559 sits modestly above them
- branch/PR hygiene check: existing open PR remains `#2409` (`perf: optimize batcher hot paths`), so no new PR was opened
Next:
- if this decode path is revisited again, compare synthetic per-kind hash cost against richer payload/access-list/authorization-list shapes or a real EIP-7702 fixture so the next experiment can tell how much of the new gap is intrinsic to transaction type versus specific field population.

## 2026-04-29 13:31 UTC
Focus: `base-protocol` synthetic signature-hash shape sensitivity across richer transaction payloads and lists.
Hypothesis: the previous same-cardinality synthetic benchmark used very small uniform transactions, so it likely understated the cost of typed payload fields; adding a second synthetic shape with large calldata plus populated access lists and authorization lists should show how much of the tx-type spread is intrinsic to the hash path versus fixture sparsity.
Commands:
- `cargo bench -p base-protocol --bench batch_reader synthetic_signature_hash_by_tx_type -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add `SyntheticSignatureHashShape`, richer synthetic fixture builders, and a new `protocol/batch_reader/synthetic_signature_hash_shape_sensitivity` Criterion group
- `cargo fmt --all`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- `cargo bench -p base-protocol --bench batch_reader synthetic_signature_hash_shape_sensitivity -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- extracted medians from `target/criterion/protocol_batch_reader_synthetic_signature_hash_shape_sensitivity/*/*/new/estimates.json`
- `gh pr list --head automation/perf-autopilot --state open`
Results:
- kept production logic unchanged and extended the synthetic hash harness with paired `simple` and `rich` shapes for all four typed transaction kinds; the `rich` shape uses `1024` bytes of calldata plus populated access lists for EIP-2930/EIP-1559 and four signed authorizations for EIP-7702
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- median signature-hash timings from the new shape-sensitivity group:
  - simple shape: `legacy` `274.22 µs` (~`268 ns/tx`), `eip2930` `274.87 µs` (~`268 ns/tx`), `eip1559` `280.25 µs` (~`274 ns/tx`), `eip7702` `536.49 µs` (~`524 ns/tx`)
  - rich shape: `legacy` `1.8150 ms` (~`1.77 µs/tx`), `eip2930` `3.0080 ms` (~`2.94 µs/tx`), `eip1559` `3.0030 ms` (~`2.93 µs/tx`), `eip7702` `3.8093 ms` (~`3.72 µs/tx`)
- interpretation: the earlier synthetic ranking was directionally right about EIP-7702 being the slowest kind, but richer payloads magnify the typed-list overhead substantially; compared with the simple shape, rich synthetic transactions raise legacy hash cost by about `6.6x`, EIP-2930 by about `10.9x`, EIP-1559 by about `10.7x`, and EIP-7702 by about `7.1x`
- this narrows the next optimization target: any future hash-focused work should treat access-list-bearing transactions and EIP-7702 separately instead of extrapolating from tiny uniform payloads alone
- branch/PR hygiene check: existing open PR remains `#2409` (`perf: optimize batcher hot paths`), so no new PR was opened
Next:
- if this decode path is revisited again, add one more synthetic sweep that varies only one shape dimension at a time (calldata size vs access-list size vs authorization-list size) so the next experiment can attribute the rich-shape slowdown to a specific hashed field rather than the combined larger fixture.

## 2026-04-29 15:41 UTC
Focus: `base-protocol` synthetic signature-hash dimension sensitivity across isolated calldata, access-list, and authorization-list expansions.
Hypothesis: the prior `simple` versus `rich` synthetic benchmark proved larger payloads and typed lists matter, but it still changed multiple fields at once; a one-factor-at-a-time benchmark should isolate which hashed field drives most of the slowdown for each typed transaction kind before attempting any production optimization around `signature_hash()`.
Commands:
- `cargo bench -p base-protocol --bench batch_reader synthetic_signature_hash_shape_sensitivity -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- edited `crates/consensus/protocol/benches/batch_reader.rs` to add new synthetic shapes (`input_heavy`, `access_list_heavy`, `authorization_heavy`) plus a `protocol/batch_reader/synthetic_signature_hash_dimension_sensitivity` Criterion group
- `cargo fmt --all`
- `cargo bench -p base-protocol --bench batch_reader synthetic_signature_hash_dimension_sensitivity -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- extracted medians from `target/criterion/protocol_batch_reader_synthetic_signature_hash_{shape_sensitivity,dimension_sensitivity}/*/*/estimates.json`
Results:
- kept production logic unchanged and extended the shared protocol bench with isolated synthetic shape knobs so each tx kind now has the smallest relevant comparison set: legacy measures `simple` vs `input_heavy`, EIP-2930/EIP-1559 add `access_list_heavy`, and EIP-7702 adds `authorization_heavy`
- validation passed: `cargo test -p base-protocol batch_reader -- --nocapture` (`2 passed`) and `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- refreshed pre-edit shape-sensitivity medians confirmed the combined `rich` shape still widened the gap substantially on this machine: legacy `272.24 µs` -> `1.8249 ms`, EIP-2930 `274.73 µs` -> `3.0109 ms`, EIP-1559 `279.87 µs` -> `3.0098 ms`, EIP-7702 `530.81 µs` -> `3.8130 ms`
- new isolated-dimension medians show calldata expansion is the largest single driver for every kind, while typed-list overhead is material but smaller than the full combined `rich` shape:
  - legacy: `simple` `280.94 µs` (~`274 ns/tx`) vs `input_heavy` `1.8088 ms` (~`1.77 µs/tx`), so nearly all of the legacy rich-shape slowdown comes from calldata size alone
  - EIP-2930: `simple` `283.16 µs` (~`277 ns/tx`), `input_heavy` `1.8169 ms` (~`1.77 µs/tx`), `access_list_heavy` `1.4907 ms` (~`1.46 µs/tx`); the combined rich shape at `3.0109 ms` is roughly additive across large calldata plus populated access lists
  - EIP-1559: `simple` `286.09 µs` (~`279 ns/tx`), `input_heavy` `1.8213 ms` (~`1.78 µs/tx`), `access_list_heavy` `1.4885 ms` (~`1.45 µs/tx`); this tracks EIP-2930 closely, suggesting access-list hashing rather than fee-field differences drives most typed-list overhead
  - EIP-7702: `simple` `531.93 µs` (~`519 ns/tx`), `input_heavy` `2.0648 ms` (~`2.02 µs/tx`), `access_list_heavy` `1.5182 ms` (~`1.48 µs/tx`), `authorization_heavy` `1.0616 ms` (~`1.04 µs/tx`); large calldata is still the biggest single contributor, but authorization hashing is a distinct secondary cost unique to 7702
- interpretation: future hash-focused work should prioritize payload-size-sensitive hashing paths first, then separately evaluate access-list and EIP-7702 authorization handling; optimizing around typed-list serialization alone is unlikely to explain the full `rich` synthetic slowdown
Next:
- if this decode path is revisited again, add a mixed two-factor synthetic sweep (for example input+access-list for EIP-2930/EIP-1559 and input+authorization for EIP-7702) or profile upstream `signature_hash()` internals so the next experiment can test whether the near-additive slowdown comes from repeated RLP length calculation, list encoding, or keccak input materialization.

## 2026-06-15 08:38 UTC
Focus: `base-protocol` two-factor synthetic signature-hash sensitivity across input+access-list and input+authorization combinations.
Hypothesis: the prior single-factor dimension benchmarks showed calldata expansion and typed-list hashing are individually expensive for `signature_hash()`; adding two-factor mixed shapes (large calldata + access lists for EIP-2930/EIP-1559, large calldata + authorization lists for EIP-7702) should show whether these costs combine additively (independent RLP encoding work) or interact (shared keccak/RLP length overhead).
Commands:
- added `InputPlusAccessList` and `InputPlusAuthorization` variants to `SyntheticSignatureHashShape` in the benchmark harness
- updated `input_len()`, `access_list_entry_count()`, and `authorization_count()` matchers for the new shapes
- extended `ACCESS_LIST_DIMENSION_SENSITIVITY_SHAPES` (EIP-2930/EIP-1559) and `EIP7702_DIMENSION_SENSITIVITY_SHAPES` with the two-factor combinations
- `cargo bench -p base-protocol --bench batch_reader synthetic_signature_hash_dimension_sensitivity -- --warm-up-time 0.2 --measurement-time 0.5 --sample-size 10`
- `cargo test -p base-protocol batch_reader -- --nocapture`
- `cargo clippy -p base-protocol --tests --benches --no-deps -- -D warnings`
- extracted median `point_estimate` values from `target/criterion/protocol_batch_reader_synthetic_signature_hash_dimension_sensitivity/*/*/new/estimates.json` and `protocol_batch_reader_synthetic_signature_hash_shape_sensitivity/*/*/new/estimates.json`
Results:
- benchmark-only change; production logic untouched
- validation passed: `2 passed` and clippy clean
- new two-factor median benchmark results (ns/tx for 1024 synthetic transactions):
  **EIP-2930**
  - `simple`: 267.8 ns/tx
  - `input_heavy`: 1769.7 ns/tx (calldata alone adds ~1475 ns/tx)
  - `access_list_heavy`: 1443.7 ns/tx (access list alone adds ~1145 ns/tx)
  - **`input_plus_access_list`: 3009.4 ns/tx**
  - predicted additive: simple + calldata_delta + alist_delta = 267.8 + 1475 + 1145 = 2887.8, versus measured 3009.4 (~4.2% above additive)
  **EIP-1559**
  - `simple`: 273.3 ns/tx
  - `input_heavy`: 1776.6 ns/tx (calldata alone adds ~1483 ns/tx)
  - `access_list_heavy`: 1456.0 ns/tx (access list alone adds ~1152 ns/tx)
  - **`input_plus_access_list`: 2942.1 ns/tx**
  - predicted additive: 273.3 + 1483 + 1152 = 2908.3, versus measured 2942.1 (~1.2% above additive)
  **EIP-7702**
  - `simple`: 518.4 ns/tx
  - `input_heavy`: 2034.3 ns/tx (calldata alone adds ~1492 ns/tx)
  - `authorization_heavy`: 1046.6 ns/tx (auth list alone adds ~517 ns/tx)
  - **`input_plus_authorization`: 2536.5 ns/tx**
  - predicted additive: 518.4 + 1492 + 517 = 2527.4, versus measured 2536.5 (~0.4% above additive)
- interpretation: the two-factor combinations are nearly additive across all three paths, which confirms that calldata hashing and typed-list hashing (access lists, authorization lists) run as independent RLP encoding and keccak steps with no material cross-field optimization opportunity; the slight super-additivity (~0.4% to ~4.2%) is consistent with one extra RLP length prefix per combined encoding pass rather than synergistic savings; EIP-1559 and EIP-7702 two-factor results are nearly perfect additive, while EIP-2930 shows a small (~4%) premium on the combined shape; these results are consistent with the combined `rich` shape from the prior run (EIP-1559/2930: ~2940 ns/tx, EIP-7702: ~3724 ns/tx), confirming internal benchmark stability
- PR hygiene: existing open PR remains `#2409`, no new PR needed
Next:
- the two-factor sweep closes the additivity question; the signature-hash path is now well-characterized at the component level and shows clear independent costs for calldata, access lists, and authorization lists; future production-optimization work would need to target upstream `signature_hash()` implementations inside the alloy crate (specifically typed-tx RLP encoding or keccak buffer handling), which is outside the scope of safe in-repo changes; if this path is revisited again, shift focus to a different service hotspot area (consensus derivation, batcher encoding, or zk polling) where local code changes can produce measurable gains

## 2026-06-15 11:40 UTC
Focus: `base-batcher-encoder` redundant per-block DA backlog recalculation in `step()` loop.
Hypothesis: `step()` recomputes `block_da_backlog_bytes(block)` for every block cursor position, duplicating an identical scan already performed in `add_block()`. Caching the per-block backlog bytes in a parallel `VecDeque<u64>` should eliminate the redundant per-step transaction-iteration cost.
Commands:
- added `block_backlog_bytes: VecDeque<u64>` field to `BatchEncoder`, populated in `add_block()` and consumed in `step()` instead of calling `block_da_backlog_bytes()`
- updated `confirm()`, `reset()`, `prune_safe()`, and `Debug` impl to maintain the parallel cache
- added `crates/batcher/encoder/benches/step_throughput.rs` with `single_batch_4096_blocks` and `span_batch_4096_blocks` Criterion benches
- `cargo test -p base-batcher-encoder -- --nocapture` — `60 passed`
- `cargo clippy -p base-batcher-encoder --tests --benches --no-deps -- -D warnings` — clean
- `cargo bench -p base-batcher-encoder --bench step_throughput -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 15`
- `cargo bench -p base-batcher-encoder --bench da_backlog -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 20`
Results:
- DA backlog getter remains O(1): `4096_blocks_pending` median 201.03 ps (~-2.18% vs prior run, p=0.00 < 0.05); `4096_blocks_half_encoded` median 201.87 ps (flat, p=0.89)
- new step throughput bench results: `single_batch_4096_blocks` median ~708 ms, `span_batch_4096_blocks` median ~1570 ms
- the step-path improvement is hard to isolate in total time because compression, channel management, and batch encoding dominate the budget; however, the eliminated per-step `block_da_backlog_bytes` call is a clear algorithmic simplification that removes O(transactions) work on every step iteration
- created PR `#3536` (the prior run had PR `#2409` which was previously open but has since been closed/merged; no active PR existed for this branch)
Next:
- revisit encoder step throughput with larger block counts to amplify the per-step savings; alternatively pivot to `ChannelOut::output_frame` or `ShadowCompressor` as the next measurable encoder bottleneck given that compression dominates the step budget

## 2026-06-15 15:05 UTC
Focus: `base-batcher-encoder` step-path component splitting to quantify composition vs compression vs channel-close framing budget.
Hypothesis: the prior end-to-end step throughput bench (~678-708ms for 4096 blocks) mixes block composition, compression/channel add_batch, and close_current_channel (flush, output_frame, and Arc-ing frames); splitting those phases should reveal whether composition or compression is the next viable micro-optimization target, or whether the channel-close path absorbs most of the budget.
Commands:
- added `crates/batcher/encoder/benches/step_components.rs` with three Criterion groups: `composition` (block_to_single_batch only), `add_batch` (write RLP-wrapped batches into a ShadowCompressor ChannelOut), and `full` (the combined step loop, sanity-checked against existing `step_throughput`)
- wired `[[bench]] name = "step_components"` in `crates/batcher/encoder/Cargo.toml`
- `cargo bench -p base-batcher-encoder --bench step_components`
- `cargo clippy -p base-batcher-encoder --tests --benches --no-deps -- -D warnings` — clean
- `cargo test -p base-batcher-encoder -- --nocapture` — `60 passed`
Results:
- composition-only (`single_4096_blocks`): median `9.52 ms` (~2.34 µs per block)
- add_batch-only (`single_4096_batches_into_channel`): median `7.35 ms` (~1.80 µs per block)
- combined composition + add_batch = ~16.87 ms, which is only ~2.5% of the full step loop
- full step (`single_4096_blocks`): median `678.66 ms`, consistent with the existing `step_throughput` bench at ~682-708ms
- interpretation: block-to-single-batch composition and per-batch compression together account for only ~2.5% of the total step budget; the remaining ~97.5% is absorbed by close_current_channel (flush, output_frame, Arc-ing frames, compression finalization) and the overall step/loop overhead; future optimization work on a per-block micro-level is unlikely to move the dial — the next meaningful gain comes from the channel-close path, specifically compression finalization and frame output during `close_current_channel`
Next:
- focus on `close_current_channel` internals: add a targeted bench that measures flush + output_frame + Arc-frame-creation together, then evaluate whether batched frame output, buffer recycling, or compressor finalization optimization is measurable
