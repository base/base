# Dowse state-prefetch benchmarks

![Dowse independent-arm canonical replay](dowse-independent-2500-2026-08-27.svg)

## Restarted, independent 2,500-block replay

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
