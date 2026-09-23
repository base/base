# Agent roles

The role definitions in this directory are opt-in instructions for a caller that
selects one. They do not apply to ordinary repository chats or user-directed
edits.

Read `AGENTS.md`, `docs/FEATURE_MAP.md`, `agents/COMMON.md`, and the selected
role file in that order. `COMMON.md` owns the autonomous-improvement and
PR/stack policy; the role files own their specialized quality bar.

| Role | Use for | Do not use for |
| --- | --- | --- |
| `simplifier` | Removing obsolete paths, duplicate logic, redundant configuration, or needless abstractions. | A cosmetic cleanup or a report that no change was found. |
| `integration_refactorer` | Simplifying ownership, state transitions, and Base-to-upstream integration boundaries. | Replacing Base-specific behavior without proving the contract is preserved. |
| `performance_engineer` | A representative, baseline-backed performance improvement. | An unmeasured optimization or a benchmark/report-only PR. |
| `test_breaker` | A confirmed behavior bug plus a deterministic regression test and fix. | Adding tests without a real behavior improvement. |
| `vertical_slice_completion` | Closing a small missing supported integration boundary. | Starting a new feature/framework. |
| `observability_truth` | Making metrics/status reflect effective live state. | Adding telemetry without an operator decision it supports. |
| `configuration_contract_breaker` | Breaking accepted-but-ignored or contradictory operator configuration. | Documentation instead of contract fixes. |
| `upstream_boundary_steward` | Simplifying Base-to-upstream integration ownership. | Replacing Base-specific behavior for shorter code. |
| `upgrade_plan_consistency` | Aligning activation, runtime schedule, config, and observability. | A second schedule or mapping layer. |
| `operator_lifecycle_guardian` | Restart, recovery, and partial-failure ownership. | Adding lifecycle machinery without a failure contract. |
| `deprecation_removal` | Removing proven obsolete paths and compatibility residue. | Unproven guard deletion. |
| `transition_state_auditor` | Auditing explicit ownership and legal/illegal transition behavior. | Cosmetic state-machine rewrites. |

## Opportunity selection principle

Prefer a candidate only when investigation identifies a concrete observable
failure, duplicated responsibility, obsolete supported surface, or measurable
cost. Let the code, current roadmap, existing PRs, and affected users/operators
identify the opportunity; do not treat a canned list of subsystems or solution
patterns as a work queue. For performance, choose the benchmark tier only after
the workload and bottleneck are understood.

## Substantive change bar

Prefer a coherent refactor, reliability/correctness fix, or feature completion
that changes a meaningful user/operator path, removes real ownership or
integration surface, or closes a supported vertical slice. Do not optimize for
the smallest possible diff or select a parser/zero-value guard merely because it
is easy. A configuration-validation-only PR is acceptable only when the accepted
configuration can cause a credible unsafe, corrupting, non-progressing, or
materially misleading production state, and the PR demonstrates that outcome.

Feature additions are allowed when they advance the current roadmap or complete
a supported capability: define the user action, integration boundary, preserved
contract, observability/operational behavior, and focused E2E or integration
evidence. Do not add a speculative framework or a feature with no supported
consumer. When the right improvement is larger, plan a small PR stack with
independently useful layers instead of shrinking the work into a superficial
guard.

## Production-quality bar

A qualifying agent PR should resemble a maintained production change: it either
fixes a reproduced end-to-end failure, removes a complete obsolete production
path, consolidates a meaningful ownership boundary, or completes a supported
operator/user capability. The diff should explain the root cause and the
before/after behavior, not merely reject a convenient invalid input.

Use the smallest coherent implementation, not the smallest possible patch. A
substantial change owns its affected lifecycle or transition end to end, updates
all required callers/tests, and leaves one canonical behavior. Prefer a focused
integration or full affected-package test in addition to a regression test. A
format, metadata, diff, or compile attempt alone is not sufficient validation;
if the relevant test cannot run in the current environment, keep the goal active
and find a viable validation route before claiming the PR is ready.
