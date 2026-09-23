# Autonomous agent roles

Use these roles only for bounded work that can produce a substantive, validated
improvement. Read `AGENTS.md` and `docs/RECENT_FEATURE_MAP.md` first.

| Role | Use for | Do not use for |
| --- | --- | --- |
| `simplifier` | Removing obsolete paths, duplicate logic, redundant configuration, or needless abstractions. | A cosmetic cleanup or a report that no change was found. |
| `integration_refactorer` | Simplifying ownership, state transitions, and Base-to-upstream integration boundaries. | Replacing Base-specific behavior without proving the contract is preserved. |
| `performance_engineer` | A representative, baseline-backed performance improvement. | An unmeasured optimization or a benchmark/report-only PR. |
| `test_breaker` | A confirmed behavior bug plus a deterministic regression test and fix. | Adding tests without a real behavior improvement. |
| `feature_historian` | An explicitly requested retrospective. | Autonomous product-improvement work. |

A role that cannot find a substantive, safely validated improvement makes no
repository change and opens no PR.

## Goal and stacked-PR policy

For an autonomous improvement run, begin with a persistent `/goal`: **open a
substantive, validated draft PR that advances the assigned product direction**.
Do not stop after rejecting the first candidate. Continue searching adjacent,
bounded opportunities until a change meets the role's quality bar and can be
validated. Do not lower that bar, create a null-result artifact, or open a
prose-only PR merely to satisfy the goal.

A larger coherent change may use a stacked PR only when it can be split into
independently reviewable, buildable steps with a real contract at each layer.
Use `gh stack init`, `gh stack add`, and `gh stack submit` to create the stack.
Each PR must state its parent/base, remain focused, pass its relevant validation,
and be useful if reviewed independently. Do not create a stack to hide an
inseparable change or to manufacture multiple PRs.
