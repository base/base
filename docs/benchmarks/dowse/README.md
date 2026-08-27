# Dowse state-prefetch benchmarks

![Dowse concurrent canonical replay](dowse-concurrent-2500-2026-08-27.svg)

## Concurrent worker race over 2,500 blocks

The production-shaped historical experiment replays Base mainnet blocks 50,492,200–50,494,699
with four direct-state-read workers and no artificial head start. Unlike the completed-cache
benchmark below, workers do not finish before execution starts. They are released after historical
signer recovery and parent-state setup, immediately before the EVM is constructed, and stop when
block execution ends. Opening each worker's state provider and every hinted read therefore race the
EVM cursor. Requested zero lead measured 3.25 µs on average and 15 µs at maximum.

The fixed range contains 2,500 blocks, 423,591 transactions, 81.5 billion gas, and two blocks above
200 Mgas. A neutral raw replay populated the host page cache. The measured arms then ran separately,
with a Base process restart before each complete arm, in this order:

1. no Dowse;
2. concurrent Dowse with 4 workers, 0 ms requested lead, and 32-account/256-slot per-transaction
   limits; and
3. a bracketing no-Dowse control.

Every replay verified the canonical block hash, complete transaction ordering, and summed gas. No
treatment alternation occurred inside an arm. The no-Dowse baseline is each block's mean across the
two controls.

| Measurement | Result |
| --- | ---: |
| Bracketed aggregate execution effect | **20.9% faster** |
| Paired-bootstrap 95% sampling interval | **20.7–21.2% faster** |
| No-Dowse control drift | 2.3% |
| Mean execution per block | 97.6 ms without Dowse; 77.2 ms with concurrent Dowse |
| Cumulative measured execution | 243.9 s without Dowse; 192.9 s with concurrent Dowse |
| Parent-state storage reads | 21.7% fewer |
| Parent-state account reads | 10.2% fewer |
| Parent-state bytecode reads | 15.2% fewer |
| Blocks faster than their bracketing baseline | 2,495 / 2,500 (99.8%) |
| Blocks regressing by more than 10 ms | 0 / 2,500 |

The bootstrap interval captures block-sampling variation only. The 2.3% control drift is the more
important bound on arm-wide host variation.

### Latency distribution and slow blocks

| Percentile | Bracketed no Dowse | Concurrent Dowse | Observed change |
| --- | ---: | ---: | ---: |
| p50 | 91.1 ms | 70.6 ms | 22.5% lower |
| p90 | 135.9 ms | 111.2 ms | 18.2% lower |
| p95 | 156.9 ms | 129.5 ms | 17.4% lower |
| p99 | 219.1 ms | 182.4 ms | 16.8% lower |

The benefit remains substantial as initial execution time increases:

| No-Dowse execution quartile | Range | Aggregate effect |
| --- | ---: | ---: |
| Fastest 25% | 39.0–75.9 ms | 23.1% faster |
| 25–50% | 75.9–91.1 ms | 22.4% faster |
| 50–75% | 91.1–109.6 ms | 21.6% faster |
| Slowest 25% | 109.7–487.7 ms | 18.6% faster |

| Block gas used | Blocks | Aggregate effect |
| --- | ---: | ---: |
| <25 Mgas | 753 | 23.5% faster |
| 25–50 Mgas | 1,542 | 21.2% faster |
| 50–100 Mgas | 188 | 16.5% faster |
| 100–200 Mgas | 15 | 13.4% faster |
| >200 Mgas | 2 | 15.2% faster |

| Block | Gas used | No Dowse | Concurrent Dowse | Observed effect |
| --- | ---: | ---: | ---: | ---: |
| 50,493,188 | 208.5 Mgas | 421.2 ms | 349.1 ms | 17.1% faster |
| 50,494,654 | 259.3 Mgas | 487.7 ms | 421.4 ms | 13.6% faster |

### Worker timing and contention

The hint planner emitted unique work for 211,916 transactions: 161,618 account targets and
2,138,551 storage targets. Workers completed all but 50 concrete targets before the EVM returned.
One of those reads completed just after execution and was not inserted, leaving 49 unattempted.
Zero reads were classified as completing before execution, so the observed result comes from
workers staying ahead of the main EVM cursor rather than from a completed cache.

Storage fetch time on the EVM thread fell 35.1%, more than the 21.7% reduction in fetch count.
Account fetch time increased 10.1% despite 10.2% fewer account fetches, evidence that concurrent
workers do create provider contention. This is why increasing worker count is not free.

The planner took 4.9 ms per block on average, but historical replay plans the complete canonical
block before releasing workers. That planning interval is not included in measured EVM execution.
In production, planning must happen incrementally as private transactions arrive and must remain off
the payload critical path.

### Parameter search

A deterministic 26-block screen covered four gas cohorts and both blocks above 200 Mgas. A
stratified 122-block validation then compared the survivors against interleaved raw controls:

| Workers | Requested lead | Reads complete before EVM | Storage-read change | Execution change | Change if lead is charged |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 0 ms | 0.0% | 21.1% fewer | 14.9% faster | 14.9% faster |
| **4** | **0 ms** | **0.0%** | **21.3% fewer** | **16.2% faster** | **16.2% faster** |
| 4 | 2 ms | 18.0% | 21.7% fewer | 15.2% faster | 13.3% faster |
| 4 | 5 ms | 51.2% | 21.8% fewer | 16.4% faster | 11.7% faster |

Eight and sixteen workers removed approximately the same number of EVM reads but performed worse
because of I/O contention. A 10 ms lead let 84.7% of reads finish early in the short screen but did
not improve execution beyond 5 ms. The selected configuration is therefore four workers with no
intentional delay. Any real lead from private-orderflow arrival is upside; payload construction
should not wait for Dowse.

The limit screen found that 8 accounts/64 slots lost material coverage, 16/128 nearly matched the
default, and increasing 32/256 to 64/512 removed only another 0.3% of storage reads. The selected
configuration therefore retains the production defaults of four workers and 32/256 limits. It does
not tune the 25 ms txpool polling interval or the 64-plan queue per worker: canonical replay knows
the final transaction list and cannot reproduce private transaction arrival, replacement, or final
sequencer ordering. Those scheduler settings need real sequencer telemetry. Event-driven planning
would be preferable to intentionally delaying payload construction.

### Interpretation

This experiment removes the earlier infinite-prefetch assumption and demonstrates that bounded
direct reads can race a single-threaded EVM successfully with zero artificial lead. It still knows
the complete canonical transaction list and order before execution. It therefore establishes the
I/O mechanism and worker count, not production hit rate from private orderflow. A sequencer trial
must measure transaction arrival lead, queue age and drops, completion before first EVM access,
exact-parent cache hits, provider contention, and Flashblock build latency.

Reth's incoming-canonical-block pre-sim remains active on the follower, but its cache is not shared
with the payload builder. Reth's separate txpool pre-sim (`--engine.txpool-prewarming`) remains
disabled. A sequencer rollout should compare neither, Dowse only, Reth txpool pre-sim only, and both
because the mechanisms are additive semantically but compete for CPU and I/O.

The corrected artifacts are preserved on the benchmark host under
`/home/brian/work/base-dowse-runtime/artifacts/dowse-concurrent-final-20260827`:

| Artifact | SHA-256 |
| --- | --- |
| `raw.jsonl` | `ca96d87ac117a10c07c28194c76225c5d311497c38bd8b104b7d312e4b071234` |
| `tuned-w4-h0-a32-s256.jsonl` | `32b9651e1950ee7f5c54b685aa8c0e0fd5407aeb49ae3f68edc4566f621dc74d` |
| `raw-bracket.jsonl` | `fb48eaa5f67d074aa0e0c856c82d47158737e2ad10ca37e1c3dd81471445e956` |
| `warmup-raw.jsonl` | `8bcc5ea307cf157178076c42ae2f17fa30595c528af4f9c4756f24885f4f8ea6` |

The fixed hint table has SHA-256
`888c58bb18035e9797610efb9d92dee17b9959c11a661926043a48c32efa01ec` and is loaded once at process
startup; this benchmark performs no online learning.

Run a neutral raw arm and restart the Base process before each measured command:

```sh
python3 docs/benchmarks/dowse/run_concurrent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant raw --output warmup-raw.jsonl

# Restart before raw, concurrent, and raw-bracket respectively.
python3 docs/benchmarks/dowse/run_concurrent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant raw --output raw.jsonl
python3 docs/benchmarks/dowse/run_concurrent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant concurrent --workers 4 \
  --head-start-us 0 --max-accounts-per-transaction 32 \
  --max-storage-slots-per-transaction 256 --output tuned.jsonl
python3 docs/benchmarks/dowse/run_concurrent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant raw --output raw-bracket.jsonl

python3 docs/benchmarks/dowse/render_independent_replay_chart.py \
  raw.jsonl tuned.jsonl chart.svg raw-bracket.jsonl
```

## Completed-cache upper-bound replay

![Dowse independent-arm canonical replay](dowse-independent-2500-2026-08-27.svg)

This benchmark replayed Base mainnet blocks 50,492,200–50,494,699: 2,500 fixed contiguous blocks,
423,591 transactions, and 81.5 billion gas. The range includes two blocks above 200 Mgas. A fixed
static Dowse hint table was loaded once at process startup and was not updated during any arm.

The node first ran an unmeasured no-Dowse pass to populate the host page cache. It then restarted
the Base process before each complete measured arm in this order:

1. no Dowse;
2. Dowse; and
3. a bracketing no-Dowse control.

No treatment alternation occurred within an arm. The no-Dowse baseline below is the per-block mean
of its two bracketing arms. Their cumulative times differed by only 0.7%, which bounds the observed
arm-order drift. Every replay verified the canonical block hash, full transaction sequence, and sum
of transaction gas against the canonical header. The range was kept entirely beyond the node's
moving state-history boundary; an earlier attempt that crossed that boundary was rejected after its
gas validation failed.

| Measurement | Result |
| --- | ---: |
| Bracketed aggregate execution effect | **19.6% faster** |
| Paired-bootstrap 95% sampling interval | **19.2–20.0% faster** |
| No-Dowse control drift | 0.7% |
| Mean execution per block | 94.0 ms without Dowse; 75.6 ms with Dowse |
| Cumulative measured execution | 235.1 s without Dowse; 189.1 s with Dowse |
| Parent-state storage reads | 22.1% fewer |
| Parent-state account reads | 10.5% fewer |
| Parent-state bytecode reads | 15.8% fewer |
| Transactions producing a hint plan | 266,664 / 423,591 (63.0%) |
| Hint planning | 4.9 ms mean per block |
| Four-worker state prefetch | 10.1 ms mean per block |

The bootstrap interval captures block-sampling variation, not an arm-wide host effect. The two
no-Dowse controls are the relevant check on that systematic risk. Planning and state reads are
outside the measured execution interval: this measures the value of a completed parent-state cache,
not serial end-to-end latency.

### Latency distribution

| Percentile | Bracketed no Dowse | Dowse | Observed change |
| --- | ---: | ---: | ---: |
| p50 | 88.1 ms | 69.2 ms | 21.5% lower |
| p90 | 129.3 ms | 110.5 ms | 14.5% lower |
| p95 | 147.2 ms | 128.6 ms | 12.7% lower |
| p99 | 202.2 ms | 189.8 ms | 6.1% lower |

Unlike the superseded paired run, this sample shows a benefit through p99. The relative gain still
decreases as blocks become slower:

| No-Dowse execution quartile | Range | Aggregate effect |
| --- | ---: | ---: |
| Fastest 25% | 41.1–73.9 ms | 23.2% faster |
| 25–50% | 73.9–88.1 ms | 21.6% faster |
| 50–75% | 88.2–106.3 ms | 20.8% faster |
| Slowest 25% | 106.3–467.4 ms | 15.8% faster |

### Effect by block gas

| Block gas used | Blocks | Aggregate effect |
| --- | ---: | ---: |
| <25 Mgas | 753 | 24.2% faster |
| 25–50 Mgas | 1,542 | 19.9% faster |
| 50–100 Mgas | 188 | 12.0% faster |
| 100–200 Mgas | 15 | 5.2% faster |
| >200 Mgas | 2 | 11.9% faster |

| Block | Gas used | No Dowse | Dowse | Observed effect |
| --- | ---: | ---: | ---: | ---: |
| 50,493,188 | 208.5 Mgas | 429.6 ms | 357.6 ms | 16.8% faster |
| 50,494,654 | 259.3 Mgas | 467.4 ms | 432.5 ms | 7.5% faster |

Two blocks are enough to show that the cache can help very large blocks, not to estimate the
above-200 Mgas population precisely.

### Interpretation

This is an idealized historical cache-effect measurement. It knows the complete canonical
transaction list and order, and it allows all four workers to finish before timing execution. It
does not establish that background workers can finish from real private-sequencer orderflow before
payload execution begins. The production experiment must measure hint lead time, queue age,
completion before first access, cache hits, and payload latency.

Same-transaction serial Dowse is no longer a candidate: the earlier replay measured planning,
state reads, and cached execution together as 1.8% slower than raw execution. Production Dowse must
remain a persistent background direct-read service.

### Interaction with Reth pre-sim

The follower's incoming-canonical-block pre-sim remains active. It starts only after a complete
block arrives, skips blocks with fewer than five transactions, and uses the process's available
parallelism. The current node does not enable
`--engine.share-execution-cache-with-payload-builder`, so that work does not supply the payload
builder's cache.

Reth's separate `--engine.txpool-prewarming` path is disabled on this node. Despite its historical
name, it is transaction pre-sim: one persistent worker sequentially EVM-executes the best pool
transactions in batches capped at 100 ms. It is not a tuned worker pool.

Dowse performs bounded non-EVM planning followed by direct parent-state reads. The current
Flashblocks experiment scans at 25 ms intervals and uses four persistent blocking workers, a queue
of 64 jobs per worker, at most 2,048 transactions per scan, limits of 32 accounts and 256 slots per
transaction, and a 256 MiB exact-parent cache. These are smoke-test settings, not tuned production
values. Dowse and Reth txpool pre-sim are semantically additive if both are enabled, but they compete
for CPU, I/O, and cache capacity. A sequencer experiment must therefore compare neither, Dowse only,
Reth txpool pre-sim only, and both.

Denim is not relevant to this measurement: the current production target is the Flashblocks
sequencer path. Wiring the later basic builder is follow-up work before that hardfork activates.

## Superseded paired replay

The earlier 50,493,300–50,495,799 benchmark executed both variants consecutively for every block and
alternated which variant ran first. The first replay averaged 1.44× the second because it populated
the exact pages consumed by the next replay. The resulting ±80% per-block spread and the reported
12.9% order-balanced estimate depend on an unverified correction for that artifact. Do not use that
run as performance evidence; `dowse-replay-2500-2026-08-26.svg` is retained only for provenance.

The initial 40-block pilot has the same paired-replay limitation and is also superseded.

## Terminology and reproduction

- **Prefetching**: non-EVM parent-state reads selected by Dowse hints.
- **Pre-sim**: speculative EVM execution. Dowse does not use pre-sim on the transaction critical
  path.

Run a neutral warmup, then restart the Base process before each measured command:

```sh
python3 docs/benchmarks/dowse/run_independent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant raw --output warmup.jsonl

# Restart Base, then run raw.jsonl. Restart again, then run dowse.jsonl. Restart once more for
# raw-bracket.jsonl.
python3 docs/benchmarks/dowse/run_independent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant raw --output raw.jsonl
python3 docs/benchmarks/dowse/run_independent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant dowse --output dowse.jsonl
python3 docs/benchmarks/dowse/run_independent_replay_arm.py \
  --start-block 50492200 --end-block 50494699 --variant raw --output raw-bracket.jsonl

python3 docs/benchmarks/dowse/render_independent_replay_chart.py \
  raw.jsonl dowse.jsonl chart.svg raw-bracket.jsonl
```

Do not benchmark a range crossing the live node's state-history retention boundary. The arm runner
aborts if replayed transaction hashes or gas differ from the canonical block.

The preserved artifacts are stored on the benchmark host under
`/home/brian/work/base-dowse-runtime/artifacts/dowse-independent-safe-20260827`:

| Artifact | SHA-256 |
| --- | --- |
| `raw.jsonl` | `c778adffe59b51b8c94e8205af73abf0cbbf012597bfcd001160c62df7760432` |
| `dowse.jsonl` | `ec6e419be7704d883b1f77db1a71a54c4fbe3a917a5cd83f7c250b298279d824` |
| `raw-bracket.jsonl` | `defc2faea1c03cf770b9a28e0d73381b7b65e08d00f289a04e4abfff20e3fd78` |
| `warmup-raw.jsonl` | `0eaa3b0813fc6ed1e827c1d0d5506848e97b00385535071b4c65daaee5f2e28a` |

The static hint table has SHA-256
`888c58bb18035e9797610efb9d92dee17b9959c11a661926043a48c32efa01ec`.

## Shadow-sequencer smoke test

A controlled mainnet-follower smoke test ran the Flashblocks builder with four persistent prefetch
workers, a 256 MiB exact-parent cache, and deterministic parent-hash A/B selection. It confirmed
that hint planning and parent-state reads run off the transaction execution path and that private
payloads are neither committed to the conductor nor gossiped. The run performed 2,382 successful
background reads for 271 planned pool transactions without worker-queue drops.

It did not produce a useful latency comparison: the follower saw only sparse public-gossip
transactions, not the sequencer's private orderflow. Shadow reconciliation also stalled because it
required a contiguous unsafe-gossip replacement range. A production-shaped A/B therefore needs
real sequencer orderflow or a reliable canonical fallback for shadow reconciliation.
