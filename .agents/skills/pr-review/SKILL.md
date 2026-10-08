---
name: pr-review
description: "Runs Base's multi-model pull request review. Use before pushing to review your branch locally (`just review`), or to change which models, prompts, reviewers, or comment formats the PR review workflow uses."
---

# PR review

One pipeline reviews a pull request in CI, once, and your local branch before you push. Each stage is a prompt file you can edit.

```
triage ─► review ──────────────────────► decide (findings) ─► comments ─┐
          council (deep changes only):                                  ├─► decide (final) ─► threads, summary
            members review ─► vote ─► chair ─► decide (findings) ─► comments ┘
```

Each reviewer's findings are decided and posted as soon as that reviewer finishes, while the others are still working. A last round then goes through the open threads and writes the summary.

1. **triage** (`agents/triage.md`) reads the change and decides the depth, `standard` or `deep`, and whether the change is block-production-sensitive. Depth depends on how hard the change is to get right, not how many lines it touches.
2. **review** runs every reviewer whose `when` matches the triage result (`review-general` always, `review-block-production` for block-production-sensitive changes).
3. **council** runs only when triage says `deep`, alongside the reviewers:
   1. Every `council-*` member reviews the whole change through its own lens (invariants, adversarial scenarios, design and conventions). The members run on models from different vendors on purpose.
   2. Each member then votes `confirm`, `reject`, or `unsure` on every finding the others reported, after checking it against the code.
   3. The `council-chair` merges the findings. Votes inform it, but it verifies a rejected finding itself rather than counting heads.
4. **decide** (`agents/decide.md`) runs more than once per review, on a fast model:
   - **A findings round** per reviewer (and one for the council), as soon as it finishes. It turns that reviewer's findings into comments, drops the ones that are wrong or already covered, and replies where an open thread already covers a problem. It posts them straight away.
   - **The final round**, when everything has finished. It goes through every open bot thread: it marks a thread whose problem the push fixed as resolved, replies where the author answered or there is something new to say, and reopens a thread it marked resolved when the problem is back. It writes the overview.

   The script allows each round only its own actions (a findings round cannot resolve or reopen a thread; the final round can also comment, for a problem no existing thread can carry, such as a thread that cannot be reopened). It skips a problem an earlier round already posted, gives a thread at most one follow-up per run, treats the comments a run posts itself as new rather than as threads "open from earlier", and keeps the inline comments of all rounds under the cap of 20. If a round fails, the final round takes that reviewer's findings and the incomplete-review banner is cleared when it posts them.

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

Comments are updated across pushes. GitHub does not let the Actions token resolve a review thread, so when a push fixes a problem the bot edits its own comment: the first line becomes "✅ Resolved by the bot" with one sentence on how it was fixed, and the original text stays underneath in a collapsed block. The thread itself stays open on GitHub for a person to close. If the problem comes back, the bot puts the original comment back and replies. A new finding never duplicates an open thread. The summary lists what was fixed in this push and what is still open, linking each thread.

The summary comment is rendered from the same structured data, so its layout does not depend on what the model writes: a headline count, a table of new findings with links to the lines, the threads still open or reopened, anything outside the diff, and a collapsed "How this was reviewed" section listing the models that ran and what the decider dropped. A summary is posted even when nothing was found: it records the reviewed commit and says how to ask for another review.

To change how comments look, edit `render.py`. To change what they say, edit `shared/finding-guide.md` or the agent prompts. If you add a severity or category, change `render.py`, the `enum`s in `schemas/`, and the guide together; `test_review.py` checks that they agree.

### Choosing the decider's model

The decider's work is mostly mechanical (turn findings into comments, compare open threads with the current code), and it runs several times per review, so it is the stage where a fast model pays off. It is also the stage that marks threads as fixed, so a weak model costs more here than anywhere: a wrong "resolved" hides a live problem.

I replayed the decider prompt from a real CI run (25 threads, 4 of them open and unfixed) on several models. Opus resolved the one thread that was fixed. Results:

| Model | Time | Result |
| --- | --- | --- |
| `opus` | 42 s | resolved the one fixed thread |
| `sonnet` | 19-28 s (three runs) | the same decision every time |
| `gpt-lts-luna` | 31 s | the same decision |
| `gemini-3.5-flash` | 48 s | resolved nothing |
| `claude-haiku-4-5-20251001` | 119 s | resolved 4 threads, 3 of which were not fixed |

That is one prompt, so it shows that Haiku is unsafe for this job and that Sonnet is a reasonable choice, not that Sonnet is equal to Opus in general. Triage and the chair showed the same speed on Sonnet as on Opus (11-13 s), so they stay on Opus. To change the decider's model, edit `agents/decide.md` and replay a real prompt first (`/tmp`-style replays work from the `*.prompt.md` files kept in the artifacts).

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
| `timeout_seconds` | Optional wall-clock limit for one attempt. Default: none. |
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
| `council-design` | council | triage says `deep` | `gemini-3.1-pro-preview` |
| `council-chair` | chair | triage says `deep` | `opus` |
| `decide` | decide | each reviewer that has findings, then once at the end | `sonnet` |

### Choosing model IDs

- **Use an alias where one exists, so the file does not go stale.** `opus`, `sonnet`, and `haiku` are resolved by the `claude` CLI to the newest model of that family (today `claude-opus-5-5`), and the summary records the model that actually ran. The alias moves when the CLI version pinned in `claude-review.yml` is bumped, so bump it deliberately and read the summary afterwards. The gateway has its own aliases ending in `-lts` (long-term support: a name that the platform team keeps pointing at a supported model) and `-latest`; `grok-lts` is the same model as `grok-4.7` today, but the CI key is not allowed to use it (see below).
- **Pin an exact ID when you want a specific model.** `gpt-lts-sol` points at `gpt-5.6-sol`, which is older than `gpt-6.1-sol`, so `council-adversary` is pinned. A pinned ID stops working when the gateway retires it, and that shows up as a failed council member.
- **The gateway decides what exists.** List it with `curl -s "$ANTHROPIC_BASE_URL/v1/models" -H "x-api-key: $ANTHROPIC_API_KEY"`, and see what an alias maps to and its output limit at `$ANTHROPIC_BASE_URL/model/info`. Some IDs are restricted (`claude-fable-5-1` returns a 403), and the CLI prints an `unrecognized_model` warning for non-Claude IDs that is harmless. There is no Muse model on this gateway today.
- **Check a model before you rely on it:** `just review --model <id>`. A model that cannot call tools or return the JSON schema fails its stage; one retry is attempted, and the full output of the failure is saved as `<agent>.failed.txt` in the artifacts.

The council spans Anthropic, OpenAI, and Google models. The chair stays on Opus because it checks other models' claims against the code. The decider runs on `sonnet`; see "Choosing the decider's model" below.

A Grok member was dropped: the gateway answers `403 Access denied to restricted model 'grok-4.7'` for the key used in CI, although it works with a developer key. To add one back, ask the platform team to allow that key to use the model, then add an agent file with `model: grok-lts`; a member that fails shows up as an "Incomplete review" banner, not a failed job.

- **Switch a model:** edit `model:` in the agent file. Use `--model` or `PR_REVIEW_MODEL` to try one locally without editing anything.
- **Add a council member or reviewer:** copy a `council-*.md` or `review-*.md` file and change the name and prompt. Nothing else needs to change. Every member votes on the others' findings automatically.
- **Remove an agent:** delete its file, keeping the rules above.
- **Change the output shape:** edit `schemas/<name>.json` and the code in `review.py` or `render.py` that consumes it. Prompts do not describe the JSON; the CLI enforces the schema.

`review-block-production` reads `docs/guides/BLOCK_PRODUCTION_REVIEW.md`. Edit that guide for what counts as a halt or stall trigger, and the agent file only for how the review is carried out.

## How long it takes

There is no time limit on an agent, and no budget for the whole run. A slow answer is still an answer, and findings are posted as each reviewer finishes, so a slow reviewer delays only its own findings and the summary. The workflow job stops at 30 minutes as a backstop. A killed job loses the final round and the summary, not the findings already posted, and the summary's status line keeps pointing at the previous review until someone comments `/review`. To limit one agent, set `timeout_seconds` in its file; an agent that hits its limit is reported as "(did not finish)" and the review goes on without it.

- **Typical run:** with medium effort (low for Gemini) and the "Working fast" guide in `shared/working-fast.md`, reviewers and council members take 30-120 seconds, and the votes, the chair and each decide round 10-40 seconds. Reviewers run in parallel, so the slowest sets when the summary appears; the first findings appear earlier.
- **Slow calls happen:** about 1 in 9 Opus calls was slow in the saved runs (5-13 minutes for under 10k output tokens, at roughly 8-11 tokens/s, against the usual 70-90). Because findings are posted as each reviewer finishes, a slow one no longer holds back the others.
- **Effort:** `max` took 5-19 minutes per agent (up to 18 tool calls). Effort and the working-fast guide are the levers; there are no time limits to raise.

## In CI

A review is expensive (about $2.50 for a standard change and $6-7 for a deep one, against about $0.60 for the reviewer this replaced), so it does not run on every push. `.github/workflows/claude-review.yml` runs it:

- **Once, when a pull request is opened or marked ready for review.** A draft is not reviewed. Marking a draft ready again does not buy a second review if the pull request already has one.
- **On request, when someone comments `/review`** on the pull request. The comment must be `/review`, alone or followed by a space or a line break and any text (so `/reviewed` and `/reviewer` do nothing) and come from a member of the Base organization who has write access to this repository. Comments from bots and from anyone else, including the pull request's author if they are not an org member, are ignored. The review that follows replaces the previous summary comment.

  A small `authorize` job decides this before the review job starts. It has no concurrency group, on purpose: GitHub keeps only one run waiting per group, so a request that will be refused (a member without write access, or a comment such as `/reviewed`) must be refused before it can take the place of an authorized `/review` that is waiting behind a review in progress.
- **Never on a push.** A push starts the `status` job instead, which runs no model: it rewrites one line of the review summary, so a reader can see that the review is out of date:

  > ⚠️ **3 commits pushed after `a1b2c3d` have not been reviewed.** [View the diff](https://github.com/base/base/compare/a1b2c3d...f4e5d6c) · comment `/review` to review them.

  The count is of the pull request's own commits after the reviewed one, so merging the base branch in does not add to it. If the branch was force-pushed so that the reviewed commit is gone, the line says the branch was rewritten and links to the whole pull request instead of a count.

Every review summary ends with how to ask for another one, and records the commit it reviewed (in an HTML comment, `pr-review:reviewed=<sha>`) so that the line above can be kept up to date. A summary from before that marker existed is left as it is.

`/review` is handled by the workflow file on the default branch, not the pull request's copy, so a pull request cannot change who may run it. It starts working once this file is merged. A review that finds the pull request has moved to a newer commit while it was starting follows it to the new head once, instead of being lost.

The review job runs on the BaseRunnerGroup runner (the one that can reach the LLM gateway). It runs `review.py --pr <number> --post`, which posts new inline comments (one review, most severe first), replies on existing bot threads, edits the bot's own comments to mark fixed threads or undo that, and one summary comment that replaces the previous one (marked `<!-- CLAUDE_REVIEW_SUMMARY -->`).

The job's token cannot resolve review threads (GitHub answers "Resource not accessible by integration"), and no other token is available to this workflow. So the bot never calls the resolve mutation. It edits its own comment instead, which the token is allowed to do, and the thread stays open for a person to close. If a token that can resolve threads is ever provided, the place to use it is `apply_plan` in `review.py`.

The script that holds the tokens comes from a clean checkout of the default branch, so a pull request cannot add or replace files in it. A `/review` comment never runs a pull request's copy: if the default branch has no pipeline yet, the job stops. A `pull_request` run falls back to the pull request's own copy only for the pull request that introduces the pipeline. Nothing from the pull request is on disk when the CLI is installed (npm reads `.npmrc` and `package.json` from the working directory), and the `status` job checks out only the default branch, because it calls the GitHub API and reads no files from the pull request. Agents run with the PR's checkout as their working directory, `--setting-sources user` (the PR's own Claude settings and hooks are not loaded), read-only tools, and no `GH_TOKEN`.

Code scanning (CodeQL) flags a workflow that checks out a pull request and then runs code in a job started by `issue_comment`. The pull request's files are read by the agents and are never executed by the job, and the script is not taken from the pull request. The first version of this workflow had two such alerts, on installing the CLI after the checkout and on the `status` job's checkout; both were changed as described above. Check the code-scanning result on the pull request after changing this workflow.

Safeguards in the script:

- It acts only on threads and summaries whose author is the Actions bot, not on any comment that contains the marker, and edits only the first comment of a bot thread. The review is of the commit it read: a push during the run does not stop it, and the summary says how many commits came after.
- It moves any comment whose line is not in the diff into the summary, and posts at most 20 inline comments (the rest go to the summary).
- Every GitHub write fails on its own. If GitHub rejects the inline review, the findings move into the summary; a failed reply or edit is reported, left out of the summary, and does not stop it. The summary is posted before the old one is deleted.
- It reads the PR's metadata and diff at one head commit, pages through all threads, and rebuilds the diff from the files API when `gh pr diff` refuses a large PR.
- If triage fails, every reviewer and the council run. If a reviewer or council member fails, the summary says the review is incomplete and the rest continue; if the chair fails, the unmerged findings go to the decider.

Running with `--post` from your machine refuses to start if the working tree has uncommitted changes to tracked files or is not at the pull request's head, because the agents read the working tree and would review code that GitHub does not have.

Run `just check::review-tests` (or `python3 .agents/skills/pr-review/test_review.py`) after changing `review.py` or `render.py`. CI runs the tests in the `metadata-checks` job, and `just pr` and `just check::all` run them too.
