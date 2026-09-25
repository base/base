---
name: performance-evidence
description: Produce representative, baseline-backed performance evidence for supported Base paths without overstating component measurements.
---

# Performance Evidence

Read `AGENTS.md`, `docs/FEATURE_MAP.md`, and the owning subsystem before
selecting a workload.

## Choose the evidence tier

1. **Microbenchmark:** use existing Criterion or `cargo bench` targets only for
   a local algorithm, allocation, conversion, or data-structure claim. It does
   not prove node, block, or TPS improvement.
2. **Transaction workload:** use `base-bench local` and a checked-in workload
   configuration for ingress, txpool, builder, execution, precompile, or
   client-visible behavior. Keep binary, YAML, sender/concurrency, gas pacing,
   block cadence, and runner identical between baseline and candidate.
3. **Snapshot/system benchmark:** use `base-bench snapshot` for execution or
   node claims that need a fixed immutable snapshot boundary. It requires **two
   separate, fresh writable datadirs**, both restored from the exact immutable
   snapshot being benchmarked: one for the builder and one for the
   validator/client node. Pass those distinct paths with the snapshot launcher
   arguments; never point both roles at one database and never run against or
   modify the immutable source snapshot. Restore a new pair for every attempt.
   See [`docs/guides/SNAPSHOT_BENCHMARKS.md`](../../../docs/guides/SNAPSHOT_BENCHMARKS.md)
   for the lifecycle and [`etc/systems/README.md`](../../../etc/systems/README.md)
   for the builder/client path requirements. Retain results, metrics, and
   metadata.
4. **State-populated benchmark:** when the target revision provides the
   `state-populate` utility, it can seed a new or otherwise empty execution
   datadir with a large, representative storage state before a workload run.
   Use it when the question is sensitive to state size or trie shape but does
   not require the historical transactions, receipts, or chain history in a
   real snapshot. Populate the disposable source datadir, verify the written
   slots and trie, then make separate writable builder and validator/client
   copies before launching a multi-node benchmark. Match its seed and sender
   count to the workload when pre-seeding load-test accounts. Do not describe a
   state-populated datadir as a faithful chain-history snapshot.

## Comparison contract

State the user or operator outcome, workload, metric, bottleneck hypothesis,
and baseline before editing. Report exact commands, commit SHAs, build profile,
runner/hardware, input/config versions, repeat count, absolute and relative
results, variance, and guardrails.

Preserve correctness and check p95/p99 latency, errors, memory, allocations,
CPU, I/O, contention, and operability. Never turn a microbenchmark result into
a system-throughput claim. Pair the result with correctness evidence from the
owning subsystem.
