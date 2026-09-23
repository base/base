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
