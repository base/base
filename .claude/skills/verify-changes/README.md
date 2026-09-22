# verify-changes CLI

A dependency-free Python CLI for a parallel, agentic change-review loop:

1. One initial decider selects allowlisted checks for a concrete Git diff.
2. Every selected check runs a check-specific decider.
3. Each applicable check invokes its own worker agent in a separate Git worktree.
4. The executor aggregates results and preserves any worker edits as patches rather than merging potentially conflicting parallel changes automatically.

The checked-in registry defines checks, command environment variables, allowed capabilities, and timeouts. Actual model/runtime wrappers are supplied through those environment variables, so the protocol remains provider-independent and can be tested without a live model. See [`SKILL.md`](SKILL.md) for setup and schemas.
