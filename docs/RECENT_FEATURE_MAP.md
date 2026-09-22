# Product Direction and Feature Map

**Current as of September 22, 2026.** This is the short product-direction guide
for contributors and agents. It is not a release note or a complete history.
Use it before proposing cross-cutting protocol, builder, proof, or operational
work.

## Direction

Base is converging on a **smaller, faster, and more testable production
system**. Prefer finishing, measuring, and deleting over adding parallel paths
or speculative capabilities.

A proposal is in direction when it does at least one of the following:

- improves a user- or operator-visible correctness, reliability, security,
  latency, throughput, or resource-use outcome;
- completes a vertical slice for an already-planned protocol capability;
- replaces or removes a superseded path, flag, abstraction, workflow, or test
  harness; or
- makes an important production behavior reproducibly observable through a
  focused E2E test, benchmark, or operational check.

Do not add a new cross-cutting protocol feature, a second implementation of an
existing path, or documentation-only cleanup unless the owning roadmap or user
explicitly asks for it.

## Active product areas

| Area | Direction | What good work looks like |
| --- | --- | --- |
| Upgrade delivery | Safely activate and operate planned upgrades. | One canonical gate, clear configuration, action/devnet coverage, and observable rollout behavior. |
| EIP-8130 and validity transactions | Finish the existing end-to-end transaction path. | Correct admission, selection, expiry, recovery, and observability across RPC, txpool, builder, execution, batching, and proofs. |
| Native assets and policies | Harden the B20/precompile product surface already being delivered. | Correct execution semantics, bounded resource use, upgrade coverage, and user-facing action/devnet evidence. |
| Proof production and disputes | Make proving and recovery dependable in production. | Reproducible flows, bounded recovery, useful metrics, and failure/restart coverage. |
| Node and operator experience | Make supported flows easier to run and diagnose. | Faster focused feedback, reliable devnet/system tests, snapshots/recovery checks, and actionable observability. |

## Sequencing and Flashblocks: deprecation plan

Flashblocks are being deprecated. Do not add new Flashblocks features, APIs,
configuration, metrics, tests, or abstractions unless they are needed to keep
an existing supported deployment safe during retirement.

The Flashblock builder is deprecated and is scheduled for removal **by October
31, 2026**, after 200 ms blocks are activated. Work in this area should:

1. prepare the 200 ms block path for safe activation;
2. migrate callers and operators away from the Flashblock builder; or
3. delete Flashblock-specific code, flags, tests, metrics, and documentation
   once the replacement path is proven.

Do **not** optimize, expand, or create new dependencies on the Flashblock
builder. Treat a proposal that needs new Flashblock behavior as out of
roadmap unless it is an explicit retirement-blocking fix.

## Simplification and performance focus

Direct simplification and performance work to these areas first. A change outside
these areas needs a clear user or operator outcome that outweighs the cost of
additional surface area.

| Priority area | Simplify | Measure | Do not do |
| --- | --- | --- | --- |
| **200 ms block path and builder** | Remove Flashblock-builder callers, flags, adapters, metrics, and test-only plumbing as migration conditions are met. Keep one canonical builder/payload path. | End-to-end block-build latency, p95/p99 timing, throughput, CPU, allocations, and memory under representative mixed transaction workloads. | New Flashblock behavior or an unmeasured builder refactor. |
| **Txpool and validity scheduling** | Collapse duplicate admission, wakeup, expiry, and invalidation paths; remove transitional indexes and compatibility code once callers move. | High-rate admission/replacement/expiry, predicate wakeup, invalidation, memory bounds, and the effect on block-build latency. | A microbenchmark that does not exercise realistic pool shape or downstream builder impact. |
| **Execution state and trie access** | Remove redundant reads, conversions, caches, and compatibility layers only when ownership and invalidation remain clear. | Representative execution/state-read workloads, allocations, peak memory, and block processing latency. | Cache additions without an eviction/invalidation contract and benchmark evidence. |
| **Proof production and recovery** | Consolidate duplicate orchestration, retry, registration, and recovery workflows; retire obsolete proving modes after migration. | Time-to-proof, queue delay, recovery time, concurrency, failure/restart behavior, and resource use. | Throughput work that hides regressions in recovery, correctness, or operator diagnosis. |
| **Snapshots, sync, and node operations** | Keep one supported operational workflow and remove duplicated configuration, metrics, and runbooks as paths converge. | Snapshot/restart/sync-to-tip time, failure recovery, disk and memory use, and actionable observability. | New operator knobs or documentation-only changes without an operational improvement. |

For every simplification, name the obsolete path and the condition that makes
its removal safe. For every performance change, establish a baseline first and
show a repeatable improvement on a representative benchmark or E2E workload.

## Decision rules for a PR

Before implementing a change, state in the PR description:

1. **User or operator outcome:** Who benefits, what currently fails or costs
   time/resources, and what observable behavior will improve?
2. **Roadmap fit:** Which area above it advances. If it touches sequencing or
   Flashblocks, explain how it supports the deprecation plan.
3. **Surface reduction:** What old path, duplicate logic, flag, workflow, or
   ongoing operational cost can be removed or avoided?
4. **Evidence:** The focused test, E2E scenario, benchmark, or operational
   check that validates the outcome. Performance claims require a representative
   baseline and repeatable comparison.
5. **Documentation need:** Add documentation only when users or operators need
   durable guidance that clear code, tests, and concise local comments cannot
   provide.

If the change cannot identify a real outcome and evidence, do not manufacture
a PR. Report the missing prerequisite, measurement, or product decision.

## Engineering priorities

1. **Delete and consolidate.** Retire superseded compatibility paths and keep
   one canonical supported workflow.
2. **Measure hot paths.** Improve builder, txpool, execution, proof, snapshot,
   and sync behavior only with representative benchmarks or E2E evidence.
3. **Exercise vertical slices.** Cross-boundary protocol changes need a stable
   path from ingress through execution and relevant batching/proof/operational
   behavior.
4. **Shorten feedback.** Prefer focused, deterministic tests that developers
   can run routinely over broad, flaky, or manual-only validation.

## Historical context

The preceding development period built the upgrade framework, B20/native
precompiles, EIP-8130, validity transactions, proof infrastructure, builder
and batcher capabilities, and operational tooling. The next phase is not to
multiply those surfaces; it is to make the supported paths converge, perform,
and remain easy to validate while retiring obsolete sequencing infrastructure.
