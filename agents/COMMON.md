# Common agent-run logic

This file applies only when a caller explicitly invokes a role from `agents/` for
an **autonomous improvement run**. It is not a repository-wide instruction for
ordinary chats, reviews, investigations, or user-directed edits.

Read `AGENTS.md`, `docs/FEATURE_MAP.md`, this file, and the selected role file
before starting work. The feature map remains authoritative for supported
product paths, Reth boundaries, and deprecations.

## Autonomous improvement contract

Create and maintain a persistent `/goal` to deliver one substantive, safely
validated improvement in the role's scope. Start with a bounded candidate and
reject it if it cannot show an observable user or operator outcome, a preserved
contract, and focused validation. Continue to another bounded candidate rather
than lowering the bar or declaring success with a report, cosmetic cleanup,
documentation-only change, benchmark-only claim, test-only change, or null
result.

Before opening a PR, confirm the diff itself demonstrates the claimed
improvement and focused validation exercises it. A role may make no repository change only when the caller explicitly asked for
an investigation rather than an autonomous improvement run.

## PR and stack policy

Open a draft PR only after the role's completion criteria are met. A larger
coherent change may use `gh stack` only when it separates into independently
reviewable, buildable, and validated PRs with a useful standalone contract.
Do not use a stack to hide an inseparable change or manufacture multiple PRs.
Each PR must state its parent/base, outcome, preserved contract, and validation.
