---
name: pr-review
description: "Runs Base's multi-agent pull request review on Amp. Use it when you are addressing review comments, to check that every comment is properly addressed before you ask for another look (`just review`, `just review-pr`). Do not run it on every push: it is expensive and not needed. Also use it to change which models, prompts, reviewers, or comment formats the review uses."
---

# PR review

One pipeline reviews a pull request in CI, once, and your local branch before you push. Each stage is a prompt file you can edit.

## When to use it

**Use it when you are addressing review comments.** After you have worked through the comments on your pull request, run it to check that each one is properly addressed. It reads the pull request's existing threads and says, for each, whether the code now fixes it, whether a reply is owed, or whether the problem is still there. That is the job it is best at, and what you should get out of it before you ask a person to look again.

```bash
just review                  # your branch, with your uncommitted changes
just review-pr 1234          # a pull request by number or URL, without checking it out
```

**Do not run it on every push.** A review runs multiple paid agent calls, and most pushes do not need one. CI reviews a pull request once, when it is opened or marked ready, and a member of the organization can ask for another with `/review`. Pushing to a branch only updates a line in the summary that counts the commits not yet reviewed. See "How long it takes" for measurements.

Other good moments: before you mark a draft ready, if the change is risky (consensus, block production, access control) and you want a second opinion first, and after a large rework.

Not needed for: a typo, a rename, a dependency bump, a comment or docs change, or a fix you have already tested and that a reviewer asked for in exactly those words.

```
initial review (medium) ─► decide (findings, medium) ─► comments
          │
          └─► selected follow-ups (low + Fast) ─► decide (findings, medium) ─► comments
                    │
                    └─► decide (final, medium) ─► threads, summary
```

Each reviewer's findings are decided and posted as soon as that reviewer finishes, while the others are still working. A last round then goes through the open threads and writes the summary.

1. **Initial review** (`agents/triage.md`, stage `triage`) always runs in `medium`. It reviews the whole change, reports findings, and selects the depth, `standard` or `deep`, and block-production sensitivity. Its findings go through a decide round even when no follow-ups are selected. Depth depends on risk and uncertainty, not line count or whether the initial review found a bug. This baseline is not a guarantee that every bug will be found.
2. **Follow-up review** runs every reviewer whose `when` matches the selection (`review-general` for deep changes, `review-block-production` for block-production-sensitive changes). Standard, non-sensitive changes need no follow-up reviewer. Follow-ups receive the routing rationale and focus areas, but not the initial findings, so they inspect the code independently.
3. **council** runs only when triage says `deep`, alongside the reviewers:
   1. Every `council-*` member reviews the whole change through its own lens (invariants, adversarial scenarios, design and conventions). Members all run in `low` with Fast serving; their prompts differ, but their model family is the same.
   2. Each member then votes `confirm`, `reject`, or `unsure` on every finding the others reported, after checking it against the code.
   3. The `council-chair` merges the findings. Votes inform it, but it verifies a rejected finding itself rather than counting heads.
4. **decide** (`agents/decide.md`) runs more than once per review, always in `medium` so a cheaper follow-up override cannot weaken verification or thread-resolution decisions:
   - **A findings round** per reviewer (and one for the council), as soon as it finishes. It turns that reviewer's findings into comments, drops the ones that are wrong or already covered, and replies where an open thread already covers a problem. It posts them straight away.
   - **The final round**, when everything has finished. It goes through every open bot thread: it marks a thread whose problem the push fixed as resolved, replies where the author answered or there is something new to say, and reopens a thread it marked resolved when the problem is back. It writes the overview.

   The script allows each round only its own actions (a findings round cannot resolve or reopen a thread; the final round can also comment, for a problem no existing thread can carry, such as a thread that cannot be reopened). It skips a problem an earlier round already posted, gives a thread at most one follow-up per run, treats the comments a run posts itself as new rather than as threads "open from earlier", and keeps the inline comments of all rounds under the cap of 20. If a round fails, the final round takes that reviewer's findings and the incomplete-review banner is cleared when it posts them.

`review.py` validates the decider's actions against the diff and the thread list, then `render.py` formats and posts them. Agents never write to GitHub. Their only tools are `read_file`, `grep` and `glob` on the pull request's files, and `submit` (see "How agents run").

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

### How agents run

Each agent is one `amp -x` run (Amp's scripted "execute" mode) with a prompt on stdin and **nothing else**: `tools_server.py`, a small MCP server, is its only tool provider, and `amp.tools.enable` is set to match, so Amp's own shell, file-editing, web and subagent tools are all off. The pipeline uses only `low` and `medium`; every low-mode invocation adds `--fast`, including council votes and retries. Medium-mode invocations do not request Fast serving.

| Tool | What it does |
| --- | --- |
| `read_file`, `grep`, `glob` | Read the pull request's files. Paths outside the checkout, and `.git`, are refused. |
| `submit` | The agent's answer. It takes the stage's schema (`schemas/*.json`) as its arguments, checks them, and tells the agent what is wrong so it can fix them and call again. |

An agent does not "return" text: it calls `submit`, and the script reads the accepted answer. Anything else it writes is discarded.

`amp` itself runs in an empty temporary directory, not in the checkout: Amp obeys an `AGENTS.md` in its working directory (a test showed it following one that said to start every reply with a made-up word), and the pull request's files must be data. The tools server reads the checkout instead. The agent's `amp` process has no `GH_TOKEN`, but it does have `AMP_API_KEY`, which it needs to run.

Each run is an Amp thread, labelled `pr-review` and private to the account whose key ran it. Every invocation passes `--visibility private`, rather than relying on the repository's sharing default. `Details` of a run's models and cost come from `amp threads usage`.

## Run it locally

```bash
just review                    # your branch vs. the main of the base/base remote; prints, posts nothing
just review --base origin/main
just review --mode low         # low + Fast follow-ups and chair; initial review and decide stay medium
just review --artifacts-dir /tmp/review # keep the exact prompts and raw results
```

It needs the `amp` CLI signed in (`amp login`, or `AMP_API_KEY`), and, to review a pull request, the `gh` CLI signed in. Uncommitted changes to tracked files are included. A deep change runs the council and costs noticeably more than a standard one. Local runs have no existing threads, so the decider only chooses what to comment on.

To preview what CI would do on an open PR, run `just review --pr 1234`. It reads the PR's real threads but still posts nothing without `--post`.

### Reviewing a pull request that is not checked out

`just review-pr 1234` (or `just review-pr https://github.com/base/base/pull/1234`) reviews that pull request without checking it out. The agents read its files from a temporary git worktree at the pull request's head commit, removed afterwards, so your branch and uncommitted changes are untouched. The pull request's own code is only read, never run. A URL names the repository too; otherwise the repository comes from `--repo` or `base/base`. `--no-worktree` reads your current checkout instead.

### If it cannot run

This is an open-source project, so the review works out of the box only for someone with an Amp account. Before any real work, `just review` runs `amp usage`, which asks Amp about the account and runs no model, so it costs nothing and fails in about a second. When something is wrong it says what and what to do, and exits with status 4, without a traceback:

```
The review could not run.
  Can't connect to ampcode.com.
  Check your internet connection, any VPN or proxy (`amp.proxy` in the Amp settings), and AMP_URL if you set it.

Nothing was posted.
```

It names the service by host only (never the full URL, which can carry a password or token), and covers: the `amp` CLI missing, signed out or with a rejected key, an account out of credit, an account not allowed to do something, a service that cannot be reached or does not resolve, a certificate error, and a busy service; and for GitHub, the `gh` CLI signed out, a bad token, a pull request or repository that does not exist, and a rate limit. The wording is in `diagnose.py`. An error that is not one of these is shown as the CLI printed it. `--skip-preflight` skips the check.

## Change a mode or a prompt

Every agent is `agents/<name>.md`: front matter, then the system prompt.

| Field | Meaning |
| --- | --- |
| `stage` | `triage`, `review`, `council`, `chair`, or `decide`. |
| `when` | `review` agents only: comma-separated `always`, `block-production`, `deep`. The agent runs if any condition holds. Default `always`. |
| `mode` | `low` or `medium` only. Amp's agent mode chooses the model, how hard it thinks, and the system prompt. Required. Low always adds `--fast`; the script enforces `medium` for `triage` and `decide`, regardless of this field or an override. |
| `timeout_seconds` | Optional wall-clock limit for one attempt. Default: none. |

The older `model`, `effort`, `tools`, `max_budget_usd` and `max_output_tokens` fields are rejected with a message saying so. The set must have exactly one `triage` and one `decide` agent and at least one `review` agent. A council needs at least two `council` members and exactly one `chair`; with no council, deep changes get only the reviewers.

Current agents (front matter for follow-ups; the script enforces the initial-review and decide modes):

| Agent | Stage | Runs when | Mode |
| --- | --- | --- | --- |
| `triage` | triage (initial review) | always | `medium` (enforced) |
| `review-general` | review | triage says `deep` | `low` + Fast |
| `review-block-production` | review | triage says block-production-sensitive | `low` + Fast |
| `council-invariants` | council | triage says `deep` | `low` + Fast |
| `council-adversary` | council | triage says `deep` | `low` + Fast |
| `council-design` | council | triage says `deep` | `low` + Fast |
| `council-chair` | chair | triage says `deep` | `medium` |
| `decide` | decide | each reviewer that has findings, then once at the end | `medium` (enforced) |

### Choosing a mode

**A mode is not a model, and Amp changes what each mode runs.** The pipeline accepts only `low` and `medium`, including through `--mode`, `PR_REVIEW_MODE`, and agent files. The summary's "How this was reviewed" table lists the models that actually ran, read from `amp threads usage` after each agent, so check it after changing a mode or upgrading the pinned CLI.

- **The council has different prompts, not different vendors.** All members use low mode. Agreement between them is not independent cross-vendor confirmation; the medium chair and decider must verify the claims against code.
- **Fast is a serving feature, not a reasoning mode.** Low agents use `amp --mode low --fast`. Amp describes Fast as faster serving for supported models, billed at a premium. Measure actual usage rather than assuming that faster means cheaper. It is separate from the "Working fast" prompt guide.
- **Cost scales with follow-up selection.** A deep review can run ten or more agent calls. The default uses low + Fast for follow-up reviewers and medium for the initial review, chair, and every decide round.
- **There is no per-agent model, effort or budget flag.** Amp's CLI has `--mode` and nothing else for this, and `--model` is not enabled for every account. If you need a specific model for one agent, that is a change to how `review.py` calls Amp, not to an agent file.
- **The decider stays in medium.** It is the stage that marks threads as fixed, so a wrong "resolved" hides a live problem. Under the old setup, a small model resolved three unfixed threads in one real prompt. A cheap follow-up override must not also downgrade the decider.

- **Switch a follow-up mode:** edit `mode:` in its agent file. Use `--mode` or `PR_REVIEW_MODE` to override follow-up reviewers and the council locally. Neither changes the enforced `medium` mode for initial review and decide. High and ultra are rejected.
- **Add a council member or reviewer:** copy a `council-*.md` or `review-*.md` file and change the name and prompt. Nothing else needs to change. Every member votes on the others' findings automatically.
- **Remove an agent:** delete its file, keeping the rules above.
- **Change the output shape:** edit `schemas/<name>.json` and the code in `review.py` or `render.py` that consumes it. Prompts do not describe the JSON; `submit` enforces the schema. `tools_server.py` understands only a few JSON Schema keywords (`SUPPORTED_KEYWORDS`); a test fails if a schema uses another, because an unknown keyword would be ignored and a bad answer would pass.

Prompts must not name tools the agent does not have (`Read`, `Bash`, `git show`, `gh pr`); a test checks this. Tell agents to `read_file`, `grep` and `glob`.

`review-block-production` reads `docs/guides/BLOCK_PRODUCTION_REVIEW.md`. Edit that guide for what counts as a halt or stall trigger, and the agent file only for how the review is carried out.

## How long it takes

There is no time limit on an agent, and no budget for the whole run. A slow answer is still an answer, and findings are posted as each reviewer finishes, so a slow reviewer delays only its own findings and the summary. The workflow job stops at 30 minutes as a backstop. A killed job loses the final round and the summary, not the findings already posted, and the summary's status line keeps pointing at the previous review until someone comments `/review`. To limit one agent, set `timeout_seconds` in its file; an agent that hits its limit is reported as "(did not finish)" and the review goes on without it.

Measured in read-only local runs on 2026-10-13 with the default modes: medium initial review, chair and decide; low + Fast follow-ups and council members. These reran the same PR heads as the earlier high-mode measurements.

| Change | Calls | Earlier cost / time | Low + Fast / medium cost / time |
| --- | ---: | --- | --- |
| Standard: [#5784](https://github.com/base/base/pull/5784), one-file hostname helper deduplication | 2 | $0.89 / 53s | $0.76 / 43s |
| Deep: [#5781](https://github.com/base/base/pull/5781), two-file RPC decoder change | 13 | $9.82 / 7m14s | $3.68 / 10m30s |
| Deep: [#5642](https://github.com/base/base/pull/5642), the older reviewer pipeline | 13 | $11.64 / 11m15s | $7.74 / 13m34s |

The earlier standard run used high initial review and decide. The earlier #5781 run used high initial review, decide and adversary, with medium for the other follow-ups. The earlier #5642 run used high initial review and decide, with low follow-ups and chair but without Fast. These comparisons change several modes at once; they do not isolate Fast's effect. Cost fell about 15%, 63% and 34%, respectively, but both deep reviews took longer.

The standard run selected no follow-ups. Its initial review took 23s/$0.40 and final decide took 19s/$0.36. Per-agent measurements for the new deep #5781 run:

| Agent / round on #5781 | Mode / actual model | Seconds | Cost |
| --- | --- | ---: | ---: |
| Initial review (`triage`) | medium / Opus 5.5 | 149 | $1.17 |
| `decide.triage` | medium / Opus 5.5 | 37 | $0.23 |
| `review-general` | low + Fast / GLM-5.3-Flash | 125 | $0.08 |
| `council-adversary` | low + Fast / GLM-5.3-Flash | 148 | $0.09 |
| `council-design` | low + Fast / GLM-5.3-Flash | 85 | $0.07 |
| `council-invariants` | low + Fast / GLM-5.3-Flash | 142 | $0.09 |
| `decide.review-general` | medium / Opus 5.5 | 31 | $0.24 |
| `council-adversary.vote` | low + Fast / GLM-5.3-Flash | 59 | $0.06 |
| `council-design.vote` | low + Fast / GLM-5.3-Flash | 126 | $0.18 |
| `council-invariants.vote` | low + Fast / GLM-5.3-Flash | 73 | $0.06 |
| `council-chair` | medium / Opus 5.5 | 106 | $0.80 |
| `decide.council` | medium / Opus 5.5 | 37 | $0.35 |
| `decide` (final) | medium / Opus 5.5 | 27 | $0.26 |

Pipeline wall times exclude preflight and checkout. Calls overlap, so their seconds do not sum to wall time. Costs sum rounded `amp threads usage` values. The three new pipelines ran concurrently, using CLI `0.0.1791867435-gdd5044`; CI's pinned CLI was not exercised. These are single samples, not latency, cost or review-quality guarantees. The block-production specialist was not selected. Larger diffs cost more even before follow-ups: #5642's medium initial review took 300s/$2.68.

All 28 agent calls completed with schema-valid answers and summaries naming the actual models. Thread metadata confirmed every call was private and archived, every low call had the `fast` feature, and medium calls did not. On #5781 the medium decider kept both existing bot threads open (including the outstanding simulation-test gap), suppressed duplicates, and produced two new comment candidates without resolving any thread. This is evidence against that particular false-resolution failure, not a general guarantee that medium will classify every fixed or unfixed thread correctly.

- **The "Working fast" guide** (`shared/working-fast.md`) caps follow-up exploring agents at about eight tool calls. The initial review is not capped: it must cover the whole change before selecting follow-ups. Votes, the chair, and decide are also uncapped because they must check all supplied findings or threads.

## In CI

A review runs several agents (about ten on a deep change), which costs much more than the single-prompt reviewer it replaced, so it does not run on every push. `.github/workflows/claude-review.yml` runs it:

- **Once, when a pull request is opened or marked ready for review.** A draft is not reviewed. Marking a draft ready again does not buy a second review if the pull request already has one.
- **On request, when someone comments `/review`** on the pull request. The comment must be `/review`, alone or followed by a space or a line break and any text (so `/reviewed` and `/reviewer` do nothing) and come from a member of the Base organization who has write access to this repository. Comments from bots and from anyone else, including the pull request's author if they are not an org member, are ignored. The review that follows replaces the previous summary comment.

  A small `authorize` job decides this before the review job starts. It also refuses a `pull_request` event for a pull request that already has a review, so that marking a draft ready a second time cannot take the waiting place of a `/review`. It has no concurrency group, on purpose: GitHub keeps only one run waiting per group, so a request that will be refused (a member without write access, or a comment such as `/reviewed`) must be refused before it can take the place of an authorized `/review` that is waiting behind a review in progress.
- **Never on a push.** A push starts the `status` job instead, which runs no model: it rewrites one line of the review summary, so a reader can see that the review is out of date:

  > ⚠️ **3 commits pushed after `a1b2c3d` have not been reviewed.** [View the diff](https://github.com/base/base/compare/a1b2c3d...f4e5d6c) · comment `/review` to review them.

  The count is of the pull request's own commits after the reviewed one, so merging the base branch in does not add to it. If the branch was force-pushed so that the reviewed commit is gone, the line says the branch was rewritten and links to the whole pull request instead of a count.

Every review summary ends with how to ask for another one, and records the commit it reviewed (in an HTML comment, `pr-review:reviewed=<sha>`) so that the line above can be kept up to date. A summary from before that marker existed is left as it is.

`/review` is handled by the workflow file on the default branch, not the pull request's copy, so a pull request cannot change who may run it. It starts working once this file is merged. A review that finds the pull request has moved to a newer commit while it was starting follows it to the new head once, instead of being lost.

The review job runs on a GitHub-hosted `ubuntu-latest` runner. Its only secret besides the job token is the repository secret `AMP_API_KEY` (an Amp access token, from https://ampcode.com/settings), and its egress allowlist adds `ampcode.com`. It installs a pinned `@ampcode/cli` from npm. It runs `review.py --pr <number> --post`, which posts new inline comments (one review, most severe first), replies on existing bot threads, edits the bot's own comments to mark fixed threads or undo that, and one summary comment that replaces the previous one (marked `<!-- CLAUDE_REVIEW_SUMMARY -->`).

The job's token cannot resolve review threads (GitHub answers "Resource not accessible by integration"), and no other token is available to this workflow. So the bot never calls the resolve mutation. It edits its own comment instead, which the token is allowed to do, and the thread stays open for a person to close. If a token that can resolve threads is ever provided, the place to use it is `apply_plan` in `review.py`.

The script that holds the tokens comes from a clean checkout of the default branch, so a pull request cannot add or replace files in it. A `/review` comment never runs a pull request's copy: if the default branch has no pipeline yet, the job stops. A `pull_request` run falls back to the pull request's own copy only for the pull request that introduces the pipeline. Nothing from the pull request is on disk when the CLI is installed (npm reads `.npmrc` and `package.json` from the working directory), and the `status` job checks out only the default branch, because it calls the GitHub API and reads no files from the pull request. Agents read the PR's checkout through `tools_server.py` (see "How agents run"), run `amp` in an empty directory so that the PR's `AGENTS.md` and settings are never loaded, and have read-only tools and no `GH_TOKEN`.

Code scanning (CodeQL) flags a workflow that checks out a pull request and then runs code in a job started by `issue_comment`. The pull request's files are read by the agents and are never executed by the job, and the script is not taken from the pull request. The first version of this workflow had two such alerts, on installing the CLI after the checkout and on the `status` job's checkout; both were changed as described above. Check the code-scanning result on the pull request after changing this workflow.

Safeguards in the script:

- It acts only on threads and summaries whose author is the Actions bot, not on any comment that contains the marker, and edits only the first comment of a bot thread. The review is of the commit it read: a push during the run does not stop it, and the summary says how many commits came after.
- It moves any comment whose line is not in the diff into the summary, and posts at most 20 inline comments (the rest go to the summary).
- Every GitHub write fails on its own. If GitHub rejects the inline review, the findings move into the summary; a failed reply or edit is reported, left out of the summary, and does not stop it. The summary is posted before the old one is deleted.
- It reads the PR's metadata and diff at one head commit, pages through all threads, and rebuilds the diff from the files API when `gh pr diff` refuses a large PR.
- If the initial review fails, every reviewer and the council run, and the summary records the incomplete initial pass. Follow-up failures never discard the initial findings. If a reviewer or council member fails, the summary says the review is incomplete and the rest continue; if the chair fails, the unmerged findings go to the decider.

Running with `--post` from your machine refuses to start if the working tree has uncommitted changes to tracked files or is not at the pull request's head, because the agents read the working tree and would review code that GitHub does not have.

Run `just check::review-tests` (or `python3 .agents/skills/pr-review/test_review.py`) after changing `review.py` or `render.py`. CI runs the tests in the `metadata-checks` job, and `just pr` and `just check::all` run them too.
