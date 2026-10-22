---
name: pr-review
description: "Runs Base's multi-model pull request review over the LLM gateway. Use it when you are addressing review comments, to check that every comment is properly addressed before you ask for another look (`just review`, `just review-pr`). Do not run it on every push: each run makes several paid model requests and most pushes do not need one. Also use it to change which models, prompts, reviewers, or comment formats the review uses."
---

# PR review

One pipeline reviews a pull request in CI, once, and your local branch before you push. Each stage is a prompt file you can edit. Every stage is one direct HTTPS request to the LLM gateway's Messages API: no Claude CLI, no agent loop, no tools, and no retries.

## When to use it

**Use it when you are addressing review comments.** After you have worked through the comments on your pull request, run it to check that each one is properly addressed. It reads the pull request's existing threads and says, for each, whether the code now fixes it, whether a reply is owed, or whether the problem is still there. That is the job it is best at, and what you should get out of it before you ask a person to look again.

```bash
just review                  # your branch, with your uncommitted changes
just review-pr 1234          # a pull request by number or URL, without checking it out
```

**Do not run it on every push.** A standard review makes 3 or 4 paid model requests and a deep one up to 11, and most pushes do not need one. CI does the same: it reviews a pull request once, when it is opened or marked ready, and a member of the organization can ask for another with `/review`. Pushing to a branch only updates a line in the summary that counts the commits not yet reviewed. Nothing reruns a review automatically; a person (or a controller acting for one) asks for each run.

Other good moments: before you mark a draft ready, if the change is risky (consensus, block production, access control) and you want a second opinion first, and after a large rework.

Not needed for: a typo, a rename, a dependency bump, a comment or docs change, or a fix you have already tested and that a reviewer asked for in exactly those words.

## How it works

The pipeline is a fixed graph. Triage selects predefined branches; no model can add steps, choose a file to read, or supply a command to run.

```
snapshot ─► triage ─┬─► reviewers (in parallel) ──────────────────────────┬─► decide (once) ─► validate ─► post (once)
                    └─► council (deep changes only, alongside reviewers):  │
                          members review ─► vote ─► chair ─────────────────┘
```

0. **Snapshot.** Before any model runs, `review.py` collects the context every stage receives: the PR title, description, changed-file list, and diff, and a source snapshot (see below).
1. **triage** (`agents/triage.md`) decides the depth, `standard` or `deep`, and whether the change is block-production-sensitive. Depth depends on how hard the change is to get right, not how many lines it touches. If triage fails, every reviewer and the council run.
2. **review** runs every reviewer whose `when` matches the triage result (`review-general` always, `review-block-production` for block-production-sensitive changes), in parallel.
3. **council** runs only when triage says `deep`, alongside the reviewers:
   1. Every `council-*` member reviews the whole change through its own lens (invariants, adversarial scenarios, design and conventions), in one request each. The members run on models from different vendors on purpose.
   2. Each member that reviewed successfully then votes `confirm`, `reject`, or `unsure` on all the findings the others reported, in one request. A member with no other member's findings to judge does not vote.
   3. The `council-chair` merges the findings, in one request. Votes inform it, but it checks a rejected finding against the supplied code rather than counting heads. With no council findings, the chair does not run.
4. **decide** (`agents/decide.md`) runs once, after every reviewer and the council have finished. It gets all findings, the names of any stages that failed, and the existing threads. It turns findings into comments, drops the ones that are wrong or already covered, replies where an open thread already covers a problem, marks a thread whose problem is fixed as resolved, reopens a thread it marked resolved when the problem is back, and writes the overview.
5. **post.** `review.py` validates the decider's actions against the diff and the thread list, then `render.py` formats them and the script posts them in one pass. Nothing is posted before the decider has finished.

Models never write to GitHub, never get a GitHub token, and have no tools: each one gets a prompt and returns JSON. The script validates the JSON against the stage's schema in `schemas/`; an answer that does not match fails that stage. It is not repaired and the request is not repeated.

### Requests per run

With the current agents:

| Run | Requests |
| --- | --- |
| standard | triage + `review-general` + decide = 3 |
| standard, block-production-sensitive | + `review-block-production` = 4 |
| deep, block-production-sensitive (the most) | triage + 2 reviewers + 3 council reviews + 3 votes + chair + decide = 11 |

A deep run that finds less makes fewer requests (no votes without other members' findings, no chair without council findings). Each number is what the client sends. The gateway may retry a request on its side; the review neither controls nor relies on that, so billed upstream calls are not guaranteed to equal this count.

### Failure handling

A failed request is reported, not retried. If a reviewer or council member fails, the summary says the review is incomplete and the rest continue. If a vote fails, the chair is told who did not vote. If the chair fails, the unmerged findings go to the decider, marked as not cross-checked. If every reviewer fails, the run stops. If the decider fails, no review actions are applied and the summary says so. To try again, ask for another run (`/review` in CI, or run the command again locally).

### The source snapshot

Models see only what the script supplies, so the snapshot is collected deterministically before inference. It contains, in this order and without duplicates:

- `AGENTS.md`, `CLAUDE.md`, and `docs/guides/BLOCK_PRODUCTION_REVIEW.md`;
- every changed path;
- every path an existing review thread points at.

Only tracked, regular, UTF-8 text files are read, from the working tree being reviewed (the temporary worktree for `just review-pr`, your checkout for `just review`). A path that is absolute, contains `..`, or goes through a symbolic link is not read. Base's root `AGENTS.md` link to `CLAUDE.md` is identified as a guidance alias referencing the separate `CLAUDE.md` entry, without following the link. Each file is line-numbered and capped (`MAX_SOURCE_FILE_CHARS`), and the source content is capped (`MAX_SOURCE_CHARS`); the diff has its own cap (`MAX_DIFF_CHARS`), all in `review.py`. A deleted, untracked, binary, other linked, or non-UTF-8 file is marked unavailable. Truncation is explicit: a file cut at its cap is marked truncated, and a cut diff or snapshot lists the files whose contents are missing or incomplete. The snapshot's omission list is capped at 8,000 characters and says when that list itself is truncated.

The prompts tell every model to use only that snapshot: not to claim it inspected code it was not given (an unchanged caller, for example), not to ask for files, commands, or another turn, and to treat missing context as evidence of neither a bug nor a fix. The decider leaves a thread open when the snapshot cannot show the fix; insufficient evidence never justifies resolving a thread.

**The tradeoff.** A bounded snapshot replaces the old tool-enabled agents, which read and searched the repository as they went. Models can no longer follow a call into an unchanged file, so a problem that only shows in code outside the snapshot can be missed, and such findings come back at low confidence. In exchange each stage is one bounded request with no exploration loop, which should make latency and cost more predictable. Neither latency nor cost of this transport has been measured yet; record real runs before quoting numbers.

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

Categories are `block-production`, `correctness`, `concurrency`, `error-handling`, `safety`, `performance`, `compatibility`, `design`, and `tests`; `shared/finding-guide.md` defines each one and the writing style (a title under 80 characters, one to three plain sentences, a concrete fix, long traces in a collapsed `Evidence` block). That guide is appended to the prompt of every agent that reports or posts findings. `shared/working-fast.md`, on working from the supplied snapshot, is appended to the reviewers' and council members' review prompts.

Comments are updated across pushes. GitHub does not let the Actions token resolve a review thread, so when a push fixes a problem the bot edits its own comment: the first line becomes "✅ Resolved by the bot" with one sentence on how it was fixed, and the original text stays underneath in a collapsed block. The thread itself stays open on GitHub for a person to close. If the problem comes back, the bot puts the original comment back and replies. A new finding never duplicates an open thread. The summary lists what was fixed in this push and what is still open, linking each thread.

The summary comment is rendered from the same structured data, so its layout does not depend on what the model writes: a headline count, a table of new findings with links to the lines, the threads still open or reopened, anything outside the diff, and a collapsed "How this was reviewed" section listing the models that ran and what the decider dropped. A summary is posted even when nothing was found: it records the reviewed commit and says how to ask for another review.

To change how comments look, edit `render.py`. To change what they say, edit `shared/finding-guide.md` or the agent prompts. If you add a severity or category, change `render.py`, the `enum`s in `schemas/`, and the guide together; `test_review.py` checks that they agree.

### Choosing the decider's model

The decider's work is mostly mechanical (turn findings into comments, compare open threads with the current code), and it runs once per review on `claude-sonnet-5-5`. It is also the stage that marks threads as fixed, so a weak model costs more here than anywhere: a wrong "resolved" hides a live problem. Before moving it to another model, replay real decider prompts (the `decide.prompt.md` file kept with `--artifacts-dir`) against threads whose outcome you know, and check that it resolves only the fixed ones.

## Run it locally

```bash
just review                    # your branch vs. the main of the base/base remote; prints, posts nothing
just review --base origin/main
just review --model claude-sonnet-5-5   # use one model for every stage
just review --artifacts-dir /tmp/review # keep the exact prompts and raw responses
just review --json             # print the decider's raw output as JSON
```

It needs two environment variables and, to review a pull request, the `gh` CLI signed in:

- `LLM_GATEWAY_HOSTNAME`: the gateway's host name, optionally with `:port`, not a URL. Requests go to `https://$LLM_GATEWAY_HOSTNAME/v1/messages`.
- `LLM_GATEWAY_API_KEY`: the key, sent as `x-api-key`.

Both are required. There is no fallback to `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, or a Claude login, and no other provider. Uncommitted changes to tracked files are included in the diff and the snapshot. A deep change runs the council and makes up to 11 requests instead of 3 or 4. Local runs have no existing threads, so the decider only chooses what to comment on.

To preview what CI would do on an open PR, run `just review --pr 1234`. It reads the PR's real threads but still posts nothing without `--post`.

### Reviewing a pull request that is not checked out

`just review-pr 1234` (or `just review-pr https://github.com/base/base/pull/1234`) reviews that pull request without checking it out. If your checkout is dirty or at another commit, the snapshot is read from a temporary git worktree at the pull request's head commit, removed afterwards, so your branch and uncommitted changes are untouched. A clean checkout already at the PR head is reused. The pull request's own code is only read, never run. A URL names the repository too; otherwise the repository comes from `--repo` or `base/base`. `--no-worktree` reads your current checkout instead, but refuses uncommitted tracked changes rather than mixing them with GitHub's diff. Use plain `just review` without `--pr` to review uncommitted edits.

### If it cannot run

Before any real work, `just review` checks the configuration: that both variables are set and that `LLM_GATEWAY_HOSTNAME` is a host name with an optional valid port. It makes no model request, so it costs nothing and cannot prove the gateway accepts the key; a wrong key or an unreachable gateway shows up on the first stage. When something is wrong it says what and what to do, and exits with status 4, without a traceback:

```
The review could not run.
  Set LLM_GATEWAY_HOSTNAME and LLM_GATEWAY_API_KEY to use the review gateway.

Nothing was posted.
```

A failed stage is explained the same way. Messages name the gateway by host only, never the port, key, or a full URL. `diagnose.py` covers: no key sent, credentials rejected, no credit or quota, a model your key may not use, a model that does not exist, a connection refused, a host that does not resolve, a timeout or dropped connection, a certificate error, a busy gateway, and a request rejected as invalid (for example `max_output_tokens` above the model's limit); and for GitHub, the `gh` CLI signed out, a bad token, a pull request or repository that does not exist, a rate limit, and a connection failure. Unknown gateway errors are reported as an HTTP status or a generic connection failure, without response headers or body. `--skip-preflight` defers the configuration check until the first stage; it does not bypass it.

## Change a model or a prompt

Every agent is `agents/<name>.md`: front matter, then the system prompt.

| Field | Meaning |
| --- | --- |
| `stage` | `triage`, `review`, `council`, `chair`, or `decide`. |
| `when` | `review` agents only: comma-separated `always`, `block-production`, `deep`. The agent runs if any condition holds. Default `always`. |
| `model` | Model ID sent in the request. Use an ID the gateway serves. |
| `effort` | `low`, `medium`, `high`, `xhigh`, or `max`, sent as the request's effort. Default `high`. |
| `timeout_seconds` | Optional HTTP timeout for the stage's one request. Default 600. |
| `max_output_tokens` | Optional output-token limit for the request. Default 16000. Gemini agents set 32000. |

There is no `tools` field: every stage is tool-free, and an agent file with `tools` or `max_budget_usd` is rejected.

The set must have exactly one `triage` and one `decide` agent and at least one `review` agent. A council needs at least two `council` members and exactly one `chair`; with no council, deep changes get only the reviewers.

Current agents (the front matter is the source of truth):

| Agent | Stage | Runs when | Model |
| --- | --- | --- | --- |
| `triage` | triage | always | `claude-opus-5-5` |
| `review-general` | review | always | `claude-opus-5-5` |
| `review-block-production` | review | triage says block-production-sensitive | `claude-opus-5-5` |
| `council-invariants` | council | triage says `deep` | `claude-opus-5-5` |
| `council-adversary` | council | triage says `deep` | `gpt-6.1-sol` |
| `council-design` | council | triage says `deep` | `gemini-3.1-pro-preview` |
| `council-chair` | chair | triage says `deep` and the council found something | `claude-opus-5-5` |
| `decide` | decide | once, at the end | `claude-sonnet-5-5` |

### Choosing model IDs

- **Use exact IDs.** The request carries the `model` field as written; nothing resolves `opus` or `sonnet` the way the Claude CLI used to, so the agent files use `claude-opus-5-5` and `claude-sonnet-5-5`. The summary records the model the gateway says answered. The gateway has its own aliases ending in `-lts` (long-term support: a name that the platform team keeps pointing at a supported model) and `-latest`; `grok-lts` is the same model as `grok-4.7` today, but the CI key is not allowed to use it (see below).
- **A pinned ID can go stale.** `gpt-lts-sol` points at `gpt-5.6-sol`, which is older than `gpt-6.1-sol`, so `council-adversary` is pinned. A pinned ID stops working when the gateway retires it, and that shows up as a failed stage.
- **The gateway decides what exists.** List it with `curl -s "https://$LLM_GATEWAY_HOSTNAME/v1/models" -H "x-api-key: $LLM_GATEWAY_API_KEY"`, and see what an alias maps to and its output limit at `https://$LLM_GATEWAY_HOSTNAME/model/info`. Some IDs are restricted (`claude-fable-5-1` returns a 403). There is no Muse model on this gateway today.
- **Check a model before you rely on it:** `just review --model <id> --artifacts-dir /tmp/review`. The model must accept the effort setting and return JSON that matches the schema in one answer. One that does not fails its stage with no repair or retry; the prompt is kept as `<agent>.prompt.md`, and any response received as `<agent>.result.json`.

The council spans Anthropic, OpenAI, and Google models. The chair stays on Opus because it checks other models' claims against the supplied code. The decider runs on `claude-sonnet-5-5`; see "Choosing the decider's model" above.

A Grok member was dropped: the gateway answers `403 Access denied to restricted model 'grok-4.7'` for the key used in CI, although it works with a developer key. To add one back, ask the platform team to allow that key to use the model, then add an agent file with `model: grok-lts`; a member that fails shows up as an "Incomplete review" banner, not a failed job.

- **Switch a model:** edit `model:` in the agent file. Use `--model` or `PR_REVIEW_MODEL` to try one locally without editing anything.
- **Add a council member or reviewer:** copy a `council-*.md` or `review-*.md` file and change the name and prompt. Nothing else needs to change. Every member votes on the others' findings automatically. Each one adds requests to every run where it applies (a council member adds a review and a vote), so update the counts above.
- **Remove an agent:** delete its file, keeping the rules above.
- **Change the output shape:** edit `schemas/<name>.json` and the code in `review.py` or `render.py` that consumes it. Prompts do not describe the JSON; the script appends the schema to the system prompt, asks the gateway for JSON-schema output, and validates the answer.
- **Change the context models see:** edit `collect_source_context` in `review.py`. The prompts assume the snapshot is the only context; keep it deterministic and its omissions marked.

`review-block-production` uses `docs/guides/BLOCK_PRODUCTION_REVIEW.md` from the snapshot. Edit that guide for what counts as a halt or stall trigger, and the agent file only for how the review is carried out.

## How long it takes

Each stage is one request with a socket timeout of 600 seconds by default (`timeout_seconds` per agent), not a wall-clock deadline for the entire stage. The longest chain is triage, then a council member's review, its vote, the chair, and the decider; reviewers run in parallel with the council. Everything is posted in one pass, at the end, so a slow stage delays the whole review. The workflow job stops at 30 minutes as a backstop. A job killed during inference posts nothing; one killed during posting can leave partial updates. Until a new summary is posted, the status line keeps pointing at the previous review. A stage that hits its timeout is reported as "(did not finish)" and the review goes on without it, except the decider, whose failure means no actions are applied.

Effort and `max_output_tokens` are the levers on how long one request takes. Timings and costs from the old tool-enabled pipeline do not apply to this one and have been removed; measure real runs before quoting new ones.

## In CI

A review makes several paid model requests, so it does not run on every push. `.github/workflows/claude-review.yml` runs it:

- **Once, when a pull request is opened or marked ready for review.** A draft is not reviewed. Marking a draft ready again does not buy a second review if the pull request already has one.
- **On request, when someone comments `/review`** on the pull request. The comment must be `/review`, alone or followed by a space or a line break and any text (so `/reviewed` and `/reviewer` do nothing) and come from a member of the Base organization who has write access to this repository. Comments from bots and from anyone else, including the pull request's author if they are not an org member, are ignored. The review that follows replaces the previous summary comment.

  A small `authorize` job decides this before the review job starts. It also refuses a `pull_request` event for a pull request that already has a review, so that marking a draft ready a second time cannot take the waiting place of a `/review`. It has no concurrency group, on purpose: GitHub keeps only one run waiting per group, so a request that will be refused (a member without write access, or a comment such as `/reviewed`) must be refused before it can take the place of an authorized `/review` that is waiting behind a review in progress.
- **Never on a push.** A push starts the `status` job instead, which runs no model: it rewrites one line of the review summary, so a reader can see that the review is out of date:

  > ⚠️ **3 commits pushed after `a1b2c3d` have not been reviewed.** [View the diff](https://github.com/base/base/compare/a1b2c3d...f4e5d6c) · comment `/review` to review them.

  The count is of the pull request's own commits after the reviewed one, so merging the base branch in does not add to it. If the branch was force-pushed so that the reviewed commit is gone, the line says the branch was rewritten and links to the whole pull request instead of a count.

Every review summary ends with how to ask for another one, and records the commit it reviewed (in an HTML comment, `pr-review:reviewed=<sha>`) so that the line above can be kept up to date. A summary from before that marker existed is left as it is.

`/review` is handled by the workflow file on the default branch, not the pull request's copy, so a pull request cannot change who may run it. A review that finds the pull request has moved to a newer commit while it was starting follows it to the new head once, instead of being lost. Neither that nor anything else reruns a review on its own initiative after it has run.

The review job runs on the BaseRunnerGroup runner (the one that can reach the LLM gateway), with `LLM_GATEWAY_HOSTNAME` from a repository variable and `LLM_GATEWAY_API_KEY` from a secret. It runs `review.py --pr <number> --post`, which posts new inline comments (one review, most severe first), replies on existing bot threads, edits the bot's own comments to mark fixed threads or undo that, and one summary comment that replaces the previous one (marked `<!-- CLAUDE_REVIEW_SUMMARY -->`).

The job's token cannot resolve review threads (GitHub answers "Resource not accessible by integration"), and no other token is available to this workflow. So the bot never calls the resolve mutation. It edits its own comment instead, which the token is allowed to do, and the thread stays open for a person to close. If a token that can resolve threads is ever provided, the place to use it is `apply_plan` in `review.py`.

The script that holds the tokens comes from a clean checkout of the default branch, so a pull request cannot add or replace files in it. A `/review` comment never runs a pull request's copy: if the default branch has no pipeline yet, the job stops. A `pull_request` run falls back to the pull request's own copy only for the pull request that introduces the pipeline. The `status` job checks out only the default branch, because it calls the GitHub API and reads no files from the pull request. The pull request's files are read by `review.py` into the snapshot and are never executed. Models receive only the prompt: no tools, no file access, and no `GH_TOKEN`; the gateway key is sent only as the request header to `LLM_GATEWAY_HOSTNAME`, and redirects are not followed.

Code scanning (CodeQL) flags a workflow that checks out a pull request and then runs code in a job started by `issue_comment`. The pull request's files are only read, and the script is not taken from the pull request. Check the code-scanning result on the pull request after changing this workflow.

Safeguards in the script:

- It acts only on threads and summaries whose author is the Actions bot, not on any comment that contains the marker, and edits only the first comment of a bot thread. The review is of the commit it read: a push during the run does not stop it, and the summary says how many commits came after.
- It accepts only actions it can validate: a `reply`, `resolve`, or `reopen` must name an existing bot thread, a thread gets at most one follow-up per run, and the comments a run posts itself are new rather than threads "open from earlier".
- It moves any comment whose line is not in the diff into the summary, and posts at most 20 inline comments (the rest go to the summary).
- Every GitHub write fails on its own. If GitHub rejects the inline review, the findings move into the summary; a failed reply or edit is reported, left out of the summary, and does not stop it. The summary is posted before the old one is deleted.
- It reads the PR's metadata and diff at one head commit, pages through all threads, and rebuilds the diff from the files API when `gh pr diff` refuses a large PR.
- If triage fails, every reviewer and the council run. If a reviewer or council member fails, the summary says the review is incomplete and the rest continue; if the chair fails, the unmerged findings go to the decider.

Running with `--post` from your machine refuses to start if the working tree has uncommitted changes to tracked files or is not at the pull request's head, because the snapshot is read from the working tree and would contain code that GitHub does not have.

Run `just check::review-tests` (or `python3 .agents/skills/pr-review/test_review.py`) after changing `review.py`, `render.py`, or `diagnose.py`. CI runs the tests in the `metadata-checks` job, and `just pr` and `just check::all` run them too.
