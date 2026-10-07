---
name: pr-review
description: "Runs Base's multi-model pull request review. Use before pushing to review your branch locally (`just review`), or to change which models, prompts, reviewers, or comment formats the PR review workflow uses."
---

# PR review

One pipeline reviews every pull request in CI and your local branch before you push. Each stage is a prompt file you can edit.

```
triage ─► review ─────────────────────────────┐
          council (deep changes only):         ├─► decide ─► comments
            members review ─► vote ─► chair ───┘
```

1. **triage** (`agents/triage.md`) reads the change and decides the depth, `standard` or `deep`, and whether the change is block-production-sensitive. Depth depends on how hard the change is to get right, not how many lines it touches.
2. **review** runs every reviewer whose `when` matches the triage result (`review-general` always, `review-block-production` for block-production-sensitive changes).
3. **council** runs only when triage says `deep`, alongside the reviewers:
   1. Every `council-*` member reviews the whole change through its own lens (invariants, adversarial scenarios, tests and compatibility).
   2. Each member then votes `confirm`, `reject`, or `unsure` on every finding the others reported, after checking it against the code.
   3. The `council-chair` merges the findings. Votes inform it, but it verifies a rejected finding itself rather than counting heads.
4. **decide** (`agents/decide.md`) reads every finding and the PR's existing review threads. It drops findings that are wrong or already covered, and it goes through every open bot thread: it resolves a thread whose problem the push fixed, replies where the author answered or there is something new to say, and reopens a resolved thread whose problem is still present.

`review.py` validates the decider's actions against the diff and the thread list, then `render.py` formats and posts them. Agents never write to GitHub; they have only `Read`, `Grep`, and `Glob`.

## What a comment looks like

Every finding has a severity and a category. The script writes the header from them, so the model supplies only the content.

> 🟠 **Major · Error handling** — Panics on empty batch
>
> `decode` unwraps the first item. An empty batch panics.
>
> **Suggested fix:** Return `Err` for an empty batch.

| Severity | Meaning |
| --- | --- |
| 🔴 `critical` | Halts or stalls block production, breaks consensus, or loses or corrupts funds or state. |
| 🟠 `major` | Will misbehave in production. |
| 🟡 `minor` | Real but low impact. |

Categories are `block-production`, `correctness`, `concurrency`, `error-handling`, `safety`, `performance`, `compatibility`, `design`, and `tests`; `shared/finding-guide.md` defines each one and the writing style (a title under 80 characters, one to three plain sentences, a concrete fix, long traces in a collapsed `Evidence` block). That guide is appended to the prompt of every agent that reports or posts findings.

Comments are updated across pushes. A thread whose problem was fixed is resolved with a one-line "Fixed" reply, a resolved thread whose problem is still there is reopened, and a new finding never duplicates an open thread. The summary lists what was fixed in this push and what is still open, linking each thread.

The summary comment is rendered from the same structured data, so its layout does not depend on what the model writes: a headline count, a table of new findings with links to the lines, the threads still open or reopened, anything outside the diff, and a collapsed "How this was reviewed" section listing the models that ran and what the decider dropped. If there is nothing to report, no summary is posted.

To change how comments look, edit `render.py`. To change what they say, edit `shared/finding-guide.md` or the agent prompts. If you add a severity or category, change `render.py`, the `enum`s in `schemas/`, and the guide together; `test_review.py` checks that they agree.

## Run it locally

```bash
just review                    # your branch vs. the main of the base/base remote; prints, posts nothing
just review --base origin/main
just review --model claude-sonnet-5-5   # use one model for every stage
just review --artifacts-dir /tmp/review # keep the exact prompts and raw results
```

It needs the `claude` CLI, signed in or with `ANTHROPIC_API_KEY` (and `ANTHROPIC_BASE_URL` if you use the LLM gateway). Uncommitted changes to tracked files are included. A deep change runs the council and costs noticeably more than a standard one. Local runs have no existing threads, so the decider only chooses what to comment on.

To preview what CI would do on an open PR, run `just review --pr 1234`. It reads the PR's real threads but still posts nothing without `--post`.

## Change a model or a prompt

Every agent is `agents/<name>.md`: front matter, then the system prompt.

| Field | Meaning |
| --- | --- |
| `stage` | `triage`, `review`, `council`, `chair`, or `decide`. |
| `when` | `review` agents only: comma-separated `always`, `block-production`, `deep`. The agent runs if any condition holds. Default `always`. |
| `model` | Model ID passed to `claude --model`. |
| `effort` | `low`, `medium`, `high`, `xhigh`, or `max`. Default `high`. |
| `tools` | Tools the agent may use. Default `Read,Grep,Glob`. Keep it read-only. |
| `timeout_seconds` | Wall-clock limit. Default 1800. |
| `max_budget_usd` | Optional spend cap for the agent. |

The set must have exactly one `triage` and one `decide` agent and at least one `review` agent. A council needs at least two `council` members and exactly one `chair`; with no council, deep changes get only the reviewers.

Current agents (the front matter is the source of truth):

| Agent | Stage | Runs when | Model |
| --- | --- | --- | --- |
| `triage` | triage | always | `claude-opus-5-5` |
| `review-general` | review | always | `claude-opus-5-5` |
| `review-block-production` | review | triage says block-production-sensitive | `claude-opus-5-5` |
| `council-invariants` | council | triage says `deep` | `claude-opus-5-5` |
| `council-adversary` | council | triage says `deep` | `gpt-6.1-sol` |
| `council-tests` | council | triage says `deep` | `grok-4.7` |
| `council-chair` | chair | triage says `deep` | `claude-opus-5-5` |
| `decide` | decide | always | `claude-opus-5-5` |

The council members run on different models on purpose: independent models make different mistakes, so a finding that several of them confirm is worth more than one a single model repeats. Which models you can use depends on your gateway; some IDs are restricted (for example `claude-fable-5-1` returned a 403 here), and the CLI prints an `unrecognized_model` warning for non-Claude IDs that is harmless. Check a model with `just review --model <id>` before putting it in a file. The chair and decider stay on Opus because they check other models' claims against the code.
- **Switch a model:** edit `model:` in the agent file. Use `--model` or `PR_REVIEW_MODEL` to try one locally without editing anything.
- **Add a council member or reviewer:** copy a `council-*.md` or `review-*.md` file and change the name and prompt. Nothing else needs to change. Every member votes on the others' findings automatically.
- **Remove an agent:** delete its file, keeping the rules above.
- **Change the output shape:** edit `schemas/<name>.json` and the code in `review.py` or `render.py` that consumes it. Prompts do not describe the JSON; the CLI enforces the schema.

`review-block-production` reads `docs/guides/BLOCK_PRODUCTION_REVIEW.md`. Edit that guide for what counts as a halt or stall trigger, and the agent file only for how the review is carried out.

## Time budget

The whole run has a wall-clock budget (`--budget-seconds`, default 4800, inside the 90-minute CI job). Each stage's `timeout_seconds` is cut down so the stages after it still fit: the council's reviews leave time for the votes, the chair, and the decider. A stage with less than a minute left is skipped and reported as failed, so the run ends with a summary rather than a killed job. Council votes are capped at 10 minutes.

## In CI

`.github/workflows/claude-review.yml` runs `review.py --pr <number> --post` on the BaseRunnerGroup runner with the LLM gateway. It posts:

- new inline comments, in one review, most severe first
- replies on existing bot threads
- a resolved thread, with a one-line "Fixed" reply, for a bot thread whose problem the push fixed
- a reopened thread, with a reason, for a resolved bot thread whose problem is still present
- one summary comment that replaces the previous one (marked `<!-- CLAUDE_REVIEW_SUMMARY -->`)

The script runs from a clean checkout of the base branch, so a PR cannot add files to the code that holds the tokens. Agents run with the PR's checkout as their working directory, `--setting-sources user` (the PR's own Claude settings and hooks are not loaded), read-only tools, and no `GH_TOKEN`.

Safeguards in the script:

- It acts only on threads and summaries whose author is the Actions bot, not on any comment that contains the marker.
- It moves any comment whose line is not in the diff into the summary, and posts at most 20 inline comments (the rest go to the summary).
- Every GitHub write fails on its own. If GitHub rejects the inline review, the findings move into the summary; a failed reply or resolve is reported but does not stop the summary. The summary is posted before the old one is deleted.
- It reads the PR's metadata and diff at one head commit, pages through all threads, and rebuilds the diff from the files API when `gh pr diff` refuses a large PR.
- If triage fails, every reviewer and the council run. If a reviewer or council member fails, the summary says the review is incomplete and the rest continue; if the chair fails, the unmerged findings go to the decider.

Run `python3 .agents/skills/pr-review/test_review.py` after changing `review.py` or `render.py`.
