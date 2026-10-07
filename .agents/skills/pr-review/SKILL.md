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
   1. Every `council-*` member reviews the whole change through its own lens (invariants, adversarial scenarios, tests and compatibility, design and conventions). The members run on models from different vendors on purpose.
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
| `max_output_tokens` | Optional. The CLI asks for 128k output tokens, which the gateway rejects for Gemini models (their limit is about 65k). Set this to 32000 for them. |

The set must have exactly one `triage` and one `decide` agent and at least one `review` agent. A council needs at least two `council` members and exactly one `chair`; with no council, deep changes get only the reviewers.

Current agents (the front matter is the source of truth):

| Agent | Stage | Runs when | Model |
| --- | --- | --- | --- |
| `triage` | triage | always | `opus` |
| `review-general` | review | always | `opus` |
| `review-block-production` | review | triage says block-production-sensitive | `opus` |
| `council-invariants` | council | triage says `deep` | `opus` |
| `council-adversary` | council | triage says `deep` | `gpt-6.1-sol` |
| `council-tests` | council | triage says `deep` | `grok-lts` |
| `council-design` | council | triage says `deep` | `gemini-3.1-pro-preview` |
| `council-chair` | chair | triage says `deep` | `opus` |
| `decide` | decide | always | `opus` |

### Choosing model IDs

- **Use an alias where one exists, so the file does not go stale.** `opus`, `sonnet`, and `haiku` are resolved by the `claude` CLI to the newest model of that family (today `claude-opus-5-5`), and the summary records the model that actually ran. The alias moves when the CLI version pinned in `claude-review.yml` is bumped, so bump it deliberately and read the summary afterwards. The gateway has its own aliases ending in `-lts` (long-term support: a name that the platform team keeps pointing at a supported model) and `-latest`; `grok-lts` is the same model as `grok-4.7` today.
- **Pin an exact ID when you want a specific model.** `gpt-lts-sol` points at `gpt-5.6-sol`, which is older than `gpt-6.1-sol`, so `council-adversary` is pinned. A pinned ID stops working when the gateway retires it, and that shows up as a failed council member.
- **The gateway decides what exists.** List it with `curl -s "$ANTHROPIC_BASE_URL/v1/models" -H "x-api-key: $ANTHROPIC_API_KEY"`, and see what an alias maps to and its output limit at `$ANTHROPIC_BASE_URL/model/info`. Some IDs are restricted (`claude-fable-5-1` returns a 403), and the CLI prints an `unrecognized_model` warning for non-Claude IDs that is harmless. There is no Muse model on this gateway today.
- **Check a model before you rely on it:** `just review --model <id>`. A model that cannot call tools or return the JSON schema fails its stage; one retry is attempted, and the full output of the failure is saved as `<agent>.failed.txt` in the artifacts.

The council spans Anthropic, OpenAI, xAI, and Google models. The chair and decider stay on Opus because they check other models' claims against the code.

- **Switch a model:** edit `model:` in the agent file. Use `--model` or `PR_REVIEW_MODEL` to try one locally without editing anything.
- **Add a council member or reviewer:** copy a `council-*.md` or `review-*.md` file and change the name and prompt. Nothing else needs to change. Every member votes on the others' findings automatically.
- **Remove an agent:** delete its file, keeping the rules above.
- **Change the output shape:** edit `schemas/<name>.json` and the code in `review.py` or `render.py` that consumes it. Prompts do not describe the JSON; the CLI enforces the schema.

`review-block-production` reads `docs/guides/BLOCK_PRODUCTION_REVIEW.md`. Edit that guide for what counts as a halt or stall trigger, and the agent file only for how the review is carried out.

## Time budget

The whole run has a wall-clock budget (`--budget-seconds`, default 4800, inside the 90-minute CI job). Each stage's `timeout_seconds` is cut down so the stages after it still fit: the council's reviews leave time for the votes, the chair, and the decider. A stage with less than a minute left is skipped and reported as failed, so the run ends with a summary rather than a killed job. Council votes are capped at 10 minutes.

## In CI

`.github/workflows/claude-review.yml` has two jobs, because the token on the BaseRunnerGroup runner (which can reach the LLM gateway) is refused when it tries to resolve or reopen a review thread.

1. **`review`** (BaseRunnerGroup) runs `review.py --pr <number> --post --handoff-file ...`. It posts new inline comments (one review, most severe first), replies on existing bot threads, and one summary comment that replaces the previous one (marked `<!-- CLAUDE_REVIEW_SUMMARY -->`). It does not change any thread's state. It writes the threads the decider wants resolved or reopened to a small JSON file, with the commit it reviewed, and uploads only that file as an artifact.
2. **`threads`** (a GitHub-hosted runner, no gateway access, never runs an agent) downloads the file and runs `review.py --pr <number> --apply-thread-actions <file>`. For each thread it resolves or reopens the thread and adds a one-line "Fixed" or "Reopened" reply. If GitHub still refuses a resolve, the reply says the problem is fixed and asks a person to resolve the thread, and later runs do not ask again.

The file comes from a job that read untrusted pull request content through a model, so the second job treats it as untrusted. It drops the whole file if the pull request has moved to a newer commit. It ignores any action that is malformed, over the limit of 50, not for a thread currently on this pull request, not started by the Actions bot, or for a thread already in the wanted state. It uses nothing from an action except the thread id and a reply of at most 2,000 characters, with the review markers removed. The most a forged file can do is resolve or reopen the bot's own threads on this pull request and add short bot replies.

Both jobs run `review.py` from a clean checkout of the base branch, so a PR cannot add files to the code that holds the tokens. Agents run with the PR's checkout as their working directory, `--setting-sources user` (the PR's own Claude settings and hooks are not loaded), read-only tools, and no `GH_TOKEN`.

Safeguards in the script:

- It acts only on threads and summaries whose author is the Actions bot, not on any comment that contains the marker.
- It moves any comment whose line is not in the diff into the summary, and posts at most 20 inline comments (the rest go to the summary).
- Every GitHub write fails on its own. If GitHub rejects the inline review, the findings move into the summary; a failed reply is reported but does not stop the summary. The summary is posted before the old one is deleted.
- It reads the PR's metadata and diff at one head commit, pages through all threads, and rebuilds the diff from the files API when `gh pr diff` refuses a large PR.
- If triage fails, every reviewer and the council run. If a reviewer or council member fails, the summary says the review is incomplete and the rest continue; if the chair fails, the unmerged findings go to the decider.

Running with `--post` and no `--handoff-file` (for example `just review --pr 1234 --post` from your machine) makes the thread changes itself, so it needs a token that is allowed to resolve threads.

Run `python3 .agents/skills/pr-review/test_review.py` after changing `review.py` or `render.py`.
