---
name: pr-review
description: "Runs Base's multi-model pull request review. Use before pushing to review your branch locally (`just review`), or to change which models, prompts, or reviewers the PR review workflow uses."
---

# PR review

One pipeline reviews every pull request in CI and your local branch before you push. It runs three stages, and each stage is a prompt file you can edit.

```
triage ──► review (one or more, in parallel) ──► decide
```

1. **triage** (`agents/triage.md`) reads the change and decides the depth, `standard` or `deep`, and whether the change is block-production-sensitive. Depth depends on how hard the change is to get right, not how many lines it touches.
2. **review** runs every reviewer whose `when` matches the triage result. Reviewers only report findings.
3. **decide** (`agents/decide.md`) reads every finding and the PR's existing review threads. It drops findings that are wrong or already covered, replies on threads that deserve a follow-up, and un-resolves a resolved thread when its problem is still present. It returns a list of actions and a replacement summary.

`review.py` validates the actions against the diff and the thread list, then posts them. Models never get write access to GitHub; reviewers have only `Read`, `Grep`, and `Glob`.

## Run it locally

```bash
just review                    # your branch vs. the main of the base/base remote; prints, posts nothing
just review --base origin/main
just review --model claude-sonnet-5-5   # use one model for every stage
just review --artifacts-dir /tmp/review # keep the exact prompts and raw results
```

It needs the `claude` CLI, signed in or with `ANTHROPIC_API_KEY` (and `ANTHROPIC_BASE_URL` if you use the LLM gateway). Uncommitted changes to tracked files are included. Local runs have no existing threads, so the decider only chooses what to comment on and what to put in the summary.

To preview what CI would do on an open PR, run `just review --pr 1234`. It reads the PR's real threads but still posts nothing without `--post`.

## Change a model or a prompt

Every agent is `agents/<name>.md`: front matter, then the system prompt.

| Field | Meaning |
| --- | --- |
| `stage` | `triage`, `review`, or `decide`. There must be exactly one `triage` and one `decide` agent. |
| `when` | `review` agents only: comma-separated `always`, `deep`, `block-production`. The agent runs if any condition holds. Default `always`. |
| `model` | Model ID passed to `claude --model`. |
| `effort` | `low`, `medium`, `high`, `xhigh`, or `max`. Default `high`. |
| `tools` | Tools the agent may use. Default `Read,Grep,Glob`. Keep it read-only. |
| `timeout_seconds` | Wall-clock limit. Default 1800. |
| `max_budget_usd` | Optional spend cap for the agent. |

Current agents:

| Agent | Stage | Runs when | Model |
| --- | --- | --- | --- |
| `triage` | triage | always | `claude-opus-5-5` |
| `review-general` | review | always | `claude-opus-5-5` |
| `review-block-production` | review | triage says block-production-sensitive | `claude-opus-5-5` |
| `review-deep` | review | triage says `deep` | `claude-opus-5-5` |
| `decide` | decide | always | `claude-opus-5-5` |

The front matter is the source of truth; this table is a summary.

- **Switch a model:** edit `model:` in the agent file. Use `--model` or `PR_REVIEW_MODEL` to try one locally without editing anything.
- **Add a reviewer:** copy a `review-*.md` file, change the name, prompt, and `when`. Nothing else needs to change. Reviewers on different models give the decider independent opinions.
- **Remove a reviewer:** delete its file.
- **Change the output shape:** edit `schemas/<stage>.json` and the code in `review.py` that consumes it. Prompts do not describe the JSON; the CLI enforces the schema.

`review-block-production` reads `docs/guides/BLOCK_PRODUCTION_REVIEW.md`. Keep editing that guide for what counts as a halt or stall trigger, and edit the agent file only for how the review is carried out.

## In CI

`.github/workflows/claude-review.yml` runs `review.py --pr <number> --post` on the BaseRunnerGroup runner with the LLM gateway. It posts:

- new inline comments, in one review
- replies on existing bot threads
- an un-resolve, with a reason, for a resolved bot thread whose problem is still present
- one summary comment that replaces the previous one (marked `<!-- CLAUDE_REVIEW_SUMMARY -->`)

The script ignores any action that targets a thread the bot did not start, any comment whose line is not in the diff (it goes into the summary instead), and more than 20 inline comments. If triage fails, every reviewer runs. If a reviewer fails, the decider is told and continues with the rest.

Run `python3 .agents/skills/pr-review/test_review.py` after changing `review.py`.
