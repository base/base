---
name: verify-changes
summary: Use an initial decider to select checks, then run each check's decider and worker agent in parallel Git worktrees.
---

# Verify Changes

Run this skill before submitting a Base change that needs agentic review. The CLI has three layers:

1. The **initial decider** sees the complete change and selects zero or more allowlisted checks.
2. Each selected check gets its own **check decider**, which decides whether that check's worker agent is needed and gives it a focused task.
3. Applicable worker agents run **in parallel**, each in an isolated Git worktree created at the target head with the source worktree's local state, including untracked files, staged as its baseline.

The first registered check is **`long-term-simplicity`**. Its decider should select its worker whenever a change adds architecture, configuration, operational process, or another lasting maintenance cost. The worker evaluates whether the change reduces concepts and duplication, clarifies ownership, and leaves a smaller long-term maintenance surface.

This supports broad checks (such as Rust correctness) and narrow ones (such as a particular consensus invariant), while preventing a decider from inventing executable commands or check IDs.

## Configure checks

The initial decider and every registered check have separate commands. Commands receive their request JSON on stdin and must emit exactly one result JSON object to stdout. They may instead write the result to `VERIFY_CHANGES_REPORT_FILE`.

```bash
export VERIFY_CHANGES_DECIDER_COMMAND='your-agent-command --role initial-decider'

# First-pass architectural-maintenance check.
export VERIFY_CHANGES_LONG_TERM_SIMPLICITY_DECIDER_COMMAND='your-agent-command --role simplicity-check-decider'
export VERIFY_CHANGES_LONG_TERM_SIMPLICITY_AGENT_COMMAND='your-agent-command --role simplicity-reviewer'

export VERIFY_CHANGES_RUST_CORRECTNESS_DECIDER_COMMAND='your-agent-command --role rust-check-decider'
export VERIFY_CHANGES_RUST_CORRECTNESS_AGENT_COMMAND='your-agent-command --role rust-reviewer'

export VERIFY_CHANGES_TARGETED_TESTS_DECIDER_COMMAND='your-agent-command --role tests-check-decider'
export VERIFY_CHANGES_TARGETED_TESTS_AGENT_COMMAND='your-agent-command --role test-worker'

# Meta-checks used when this skill, its CLI, or its decision contracts change.
export VERIFY_CHANGES_CLI_DECIDER_COMMAND='your-agent-command --role verify-changes-cli-decider'
export VERIFY_CHANGES_CLI_AGENT_COMMAND='your-agent-command --role verify-changes-cli-reviewer'
export VERIFY_CHANGES_DECIDER_CONTRACTS_DECIDER_COMMAND='your-agent-command --role decider-contracts-decider'
export VERIFY_CHANGES_DECIDER_CONTRACTS_AGENT_COMMAND='your-agent-command --role decider-contracts-reviewer'
```

Every command runs from the applicable repository worktree and receives:

- `VERIFY_CHANGES_REQUEST_FILE`: path to the full request JSON.
- `VERIFY_CHANGES_REPORT_FILE`: path where it may write its result JSON.
- `VERIFY_CHANGES_AGENT_ID`: `initial-decider`, `<check>-decider`, or the check ID.
- `VERIFY_CHANGES_WRITE_POLICY`: `deny` for initial/check deciders; `allow` for worker agents by default, or `deny` for workers with `--read-only`.

A worker agent with the default write policy can inspect code, run tests, edit code, and add tests. Its worktree is isolated from every other check and from the source worktree. The CLI stores any changes made after that baseline as `checks/<check-id>/agent.patch` in the artifacts directory; it never implicitly applies parallel worker patches to the source worktree.

## Run it

```bash
python3 .claude/skills/verify-changes/verify_changes.py \
  --base refs/remotes/base/main --head HEAD \
  --max-parallel 4 --artifacts-dir .verify-changes
```

Useful options:

```bash
# Invoke and validate only the initial decider.
python3 .claude/skills/verify-changes/verify_changes.py --dry-run

# Request non-modifying agents. This is policy, not an OS sandbox.
python3 .claude/skills/verify-changes/verify_changes.py --read-only

# Keep worker worktrees after execution for debugging or manual patch inspection.
python3 .claude/skills/verify-changes/verify_changes.py --keep-worktrees

# Write the aggregate machine-readable report elsewhere.
python3 .claude/skills/verify-changes/verify_changes.py --report-file /tmp/review.json
```

The command exits nonzero for malformed agent output, required check/agent failures, missing required commands, or blocking findings. It writes all protocol requests, stdout/stderr logs, results, captured patches, and an aggregate report below the artifacts directory.

## Response schemas

The initial decider returns only allowlisted check IDs:

```json
{
  "summary": "Rust and test execution are relevant",
  "checks": [
    {"id": "rust-correctness", "task": "Review changed Rust APIs", "required": true},
    {"id": "targeted-tests", "task": "Run affected crate tests", "required": true}
  ]
}
```

Each selected check's decider returns:

```json
{
  "summary": "The change affects public Rust behavior",
  "run_agent": true,
  "agent_task": "Review API compatibility and error behavior"
}
```

Each worker agent returns:

```json
{
  "status": "pass",
  "summary": "Reviewed the API and ran focused tests",
  "findings": [],
  "commands_run": ["cargo test -p affected-crate"]
}
```

Worker statuses are `pass`, `needs_changes`, `fail`, and `error`. A required worker status other than `pass`, or any finding with `blocking: true`, makes verification fail.
