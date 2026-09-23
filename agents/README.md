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

## Opportunity selection principle

Prefer a candidate only when investigation identifies a concrete observable
failure, duplicated responsibility, obsolete supported surface, or measurable
cost. Let the code, current roadmap, existing PRs, and affected users/operators
identify the opportunity; do not treat a canned list of subsystems or solution
patterns as a work queue. For performance, choose the benchmark tier only after
the workload and bottleneck are understood.
