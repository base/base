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

## Legacy OP and transition-system convergence

A second simplification goal is to retire inherited Optimism-era behavior that
predates Holocene. Do not preserve old special cases, flags, mappings, or
compatibility branches merely because they are established. Remove them when
the supported post-Holocene behavior and migration condition are explicit and
covered by focused tests.

For upgrade, derivation, execution, and operator flows, make state transitions
explicit rather than implicit in scattered conditionals:

- define the meaningful states, inputs/events, legal transitions, terminal
  conditions, and rejection behavior;
- keep one canonical transition owner instead of translating the same state
  across multiple adapters;
- expose the active state, transition reason, and failure reason through useful
  metrics, logs, or operator-facing status; and
- derive hardfork planning from a declarative canonical schedule with clear
  activation, rollback/recovery, test, and observability requirements.

Improve abstractions only when they make the supported state machine or
ownership boundary clearer and remove real duplication. Avoid generic wrappers
that conceal transition state, hardfork conditions, or Base-vs-upstream
responsibility.

## Decision rules for a PR

Before implementing a change, state in the PR description:

1. **User or operator outcome:** Who benefits, what currently fails or costs
   time/resources, and what observable behavior will improve?
2. **Roadmap fit:** Which product constraint, deprecation commitment, or
   supported path it respects. If it touches sequencing or Flashblocks, explain
   how it supports the retirement plan.
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
2. **Measure supported hot paths.** Pursue performance work only where it
   serves a supported product path and has representative benchmark or E2E
   evidence; do not optimize deprecated infrastructure.
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
