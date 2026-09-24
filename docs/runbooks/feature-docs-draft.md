# Runbook: automatic feature-verification doc drafts

This runbook operates the default-off automation that opens at most one
human-reviewed **draft** documentation pull request per merged source PR,
proposing an update to `.agents/skills/verify-base/references/features/`.
It does not describe how the feature works internally (see
`etc/scripts/ci/feature_docs.py` and `.github/workflows/feature-docs-draft.yml`
for that); it describes how an operator enables, tests, monitors, and
recovers it.

**What this automation is not.** It is not a fully automatic catch-up tool
for the existing backlog of undocumented behavior, and it is not a
guarantee of exact semantic deduplication against existing pages. It opens
at most one draft per merged source PR, using deterministic caps and an
imperfect same-page collision check; a human reviewer, not the automation,
is the final check that a proposed page is correct, complete, and
non-duplicate. Generated pull requests are **always** opened as drafts —
never ready for review, never auto-merged — regardless of mode or cap
state.

## 1. Configuration and defaults

All configuration is repository variables/secrets under the
`FEATURE_DOCS_*` prefix, plus the existing `LLM_GATEWAY_*` values.
Automation defaults off; missing required identity or credentials fails closed.

| Name | Kind | Default | Purpose |
| --- | --- | --- | --- |
| `FEATURE_DOCS_AUTOMATION_ENABLED` | variable | `false` | Master switch. Gates automatic launch on merge and both App-token minting and remote-write steps in `publish`. Read-only dry-run preparation still runs while this is `false`. |
| `FEATURE_DOCS_APP_SLUG` | variable | (none) | The App slug **without** `[bot]`, e.g. `feature-docs`. Required for recursion, cap inventory, and publication/recovery ownership checks; the script derives the bot login. |
| `FEATURE_DOCS_APP_ID` | secret | (none) | GitHub App ID used to mint a short-lived installation token in `publish`. |
| `FEATURE_DOCS_APP_PRIVATE_KEY` | secret | (none) | GitHub App private key paired with `FEATURE_DOCS_APP_ID`. |
| `FEATURE_DOCS_MODEL` | variable | `claude-opus-4-6-default` | Model identifier sent to the LLM gateway, matching `claude-review.yml`'s default. |
| `FEATURE_DOCS_ASSIGNEE` | variable | (none; must be set before enabling) | The exact maintainer login assigned to every generated draft. There is no `CODEOWNERS` file in this repository and draft PRs suppress CODEOWNERS auto-review suggestions regardless, so this explicit assignment is the only reviewer-visibility mechanism; it does not imply required-reviewer enforcement. |
| `LLM_GATEWAY_HOSTNAME` / `LLM_GATEWAY_API_KEY` | existing variable/secret | (existing) | Reused as-is for the tool-less inference call. |

Size, time, and queue caps (implemented in `etc/scripts/ci/feature_docs.py`,
not operator-configurable per run): `MAX_PR_FILES` (3000 across ≤30 pages,
the GitHub API pagination ceiling only), `MAX_INFERENCE_FILES` (100),
`MAX_EVIDENCE_TEXT_BYTES` (200 KiB aggregate), `MAX_TEXT_BYTES` (64 KiB per
file), `MAX_PAGE_BYTES` (64 000), `MAX_OPEN` (3), `DAILY_LIMIT` (3, UTC,
counting app-authored deterministic-branch PRs created today including
closed/merged ones), and the GitHub Actions `queue: max` 100-pending-run
cap.

GitHub documents `queue: max` and its 100-pending limit in
[Control workflow concurrency](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency).
It requires `cancel-in-progress: false`; older workflow linters may not yet
recognize this supported key.

## 2. Generated command safety grammar

The schema validator does not merely scan `page_markdown` for banned
substrings; it recognizes a narrow, deterministic **supported command
grammar** for the fenced-shell and inline-code command forms a generated
page is allowed to contain, modeled on the exact commands already used by
`local-devnet-transaction-inclusion.md`. This mirrors the local devnet
verification lifecycle only — it is not a general-purpose shell-safety
classifier.

**Supported (kept as-is, byte-for-byte, when evidenced by the snapshot):**

* the local devnet lifecycle via `just`, e.g. `just devnet up-single`,
  `just devnet ps`, `just devnet logs base-builder`, `just devnet down` —
  never an arbitrary `just` recipe, and never a deploy, publish, or
  host-cleaning recipe;
* `cast send` / `cast block` against a `127.0.0.1`/`localhost` RPC URL
  only — never a remote host;
* `cargo test -p base-<package>` scoped to a known package/target
  already present in the workspace — never an unscoped or wildcard
  invocation that could touch code outside the cited evidence;
* `source etc/docker/devnet-env` verbatim — the exact, existing path; not
  a substitute or generated env file;
* multiline commands using a trailing backslash continuation, and
  double-quoted `"$ANVIL_ACCOUNT_1_KEY"`/`"$ANVIL_ACCOUNT_2_ADDR"`-style
  **references** to the values `etc/docker/devnet-env` already defines —
  never a literal private key, address, or other credential value inlined
  in the page.

**Refused outright (the whole `add`/`update` is rejected, not sanitized):**
shell operators and substitution (`;`, `&&`, `||`, backticks, `$(...)`,
pipes into another interpreter, redirection); any `curl`/`wget`-style or
otherwise unbounded remote network fetch; raw HTML or script tags; and any
literal credential, secret, or private-key-shaped value.

**Anything the grammar does not recognize causes an abstention** — the
automation does not attempt to guess whether an unfamiliar command is
safe. A human can still write that page manually; the automation simply
declines to draft one for a behavior whose verification steps do not fit
this narrow, evidenced grammar. This is a conservative allowlist, not a
universal semantic-safety guarantee: it recognizes a fixed vocabulary of
known-safe local verification commands and rejects everything else,
including commands that a human might judge safe on inspection.

## 3. Pre-enable gates

Complete every gate below with `FEATURE_DOCS_AUTOMATION_ENABLED=false`
before flipping it to `true`. None of these are exercised by this
documentation change; they are deployment prerequisites, and unavailable
credentials in a given environment must not block the unit-level
implementation or its tests.

1. **Unit tests.** `just feature-docs-test` (CI mirror:
   `.github/workflows/feature-docs-test.yml`) must pass. This is
   credential-free and does not depend on any gate below.
2. **Gateway protocol smoke test.** Confirm `POST
   {ANTHROPIC_BASE_URL}/v1/messages` against the real configured
   `LLM_GATEWAY_HOSTNAME`/`FEATURE_DOCS_MODEL` returns the expected
   `content[].text` shape and that strict-JSON parsing succeeds. Do not
   print `LLM_GATEWAY_API_KEY` or any token in smoke-test output or logs.
3. **Harden-runner egress allowlist.** Run the `infer` job once (dry run)
   with `egress-policy: audit` to capture a harden-runner insights report,
   then confirm the exact artifact-download and artifact-upload endpoints
   your runner uses before switching that job to `egress-policy: block`.
   The representative endpoint set in the workflow is a starting point,
   not a verified answer for every runner pool.
4. **App installation and permission test.** Install the GitHub App scoped
   to **only** `base/base`, with `contents: write`, `pull-requests:
   write`, and `issues: write` (the last only for the named-maintainer
   assignment; drop it if that mechanism is dropped). Mint a token and
   confirm it can push a branch, open a draft PR, and assign
   `FEATURE_DOCS_ASSIGNEE`. Verify applicable branch protections and signing
   requirements apply to the App; do not grant a ruleset bypass. Installation
   permissions do not restrict writes to these two documentation paths; the
   deterministic publisher enforces that boundary and never calls a merge API.
5. **CI, Depot, and signing smoke test.** Confirm a branch pushed by the
   App token triggers required CI and any Depot-triggered workflows the
   same way a human-pushed branch does, and that the repository's commit
   signing/branch-protection requirements are satisfied by the App's
   commits. If signing is required and the App cannot satisfy it, do not
   bypass or weaken the signing requirement to enable this automation —
   fix the App configuration instead.
6. **Historical replay in dry run.** Pick several already-merged PRs and
   replay them with automation disabled, using the manual dry run
   (§5), to inspect evidence boundaries and proposed pages before any
   live publish.
7. **Snapshot-drift abort test.** Confirm a `publish` run aborts cleanly,
   with a replay instruction in the run summary, when `main` moves
   between snapshot capture and the publish step (§7).

Only after every gate above passes, and a real `FEATURE_DOCS_ASSIGNEE` is
chosen, set `FEATURE_DOCS_AUTOMATION_ENABLED=true`.

## 4. Normal operation

On merge of a source PR into `main`, the trusted workflow captures the
current reviewed tip of `main` as both `control_sha` (where the automation
scripts are read from) and `snapshot_sha` (where current page/source text
is read from), collects evidence, asks the model for one structured
decision, and — only for a validated `add`/`update` — renders and opens
one draft PR. Every non-publishing outcome (`no_op`, `abstain`, a cap hit,
or automation disabled) is visible only in the run summary; the bot posts
no PR comment.

The source key that identifies a run is **`(base/base repository, source
PR number, source PR merge SHA)`**, encoded in the deterministic branch
name `automation/verify-base/<pr>-<merge-sha>`. A generated PR changes **at
most two paths**: the one feature page under
`.agents/skills/verify-base/references/features/<slug>.md` and the shared
index `.agents/skills/verify-base/references/features/README.md`. A
page-only update (no index text change) stages only the page; anything
outside those two paths is a hard error, never a silent extra edit.

## 5. Manual dry run and explicit publish replay

`workflow_dispatch` on `feature-docs-draft.yml` takes `source_pr` (required)
and `dry_run` (boolean, default `true`), and is only accepted when
dispatched from `refs/heads/main`.

* **Dry run (preview only, safe regardless of the enablement flag):**
  ```sh
  gh workflow run feature-docs-draft.yml --ref main -f source_pr=N -F dry_run=true
  ```
  This runs collection, inference, and the read-only `prepare` preflight
  (digest verification, schema validation, page/index rendering into a
  proposed-patch summary) but never mints an App token and never pushes or
  opens a PR.
* **Explicit publish replay (writes, requires
  `FEATURE_DOCS_AUTOMATION_ENABLED=true`):**
  ```sh
  gh workflow run feature-docs-draft.yml --ref main -f source_pr=N -F dry_run=false
  ```
  Use this to reprocess one known merged PR — for example after a snapshot
  drift abort, an inference-budget abstention, or a queue-overflow drop.
  It is a fresh run: it re-collects, re-infers, and re-renders from
  scratch. It does not know about, and will not adopt, a previously pushed
  but not-yet-PR'd branch (see §8 for that recovery path); if a
  deterministic branch for the same source key already exists, the fresh
  run abstains on collision rather than overwriting it.

## 6. Caps, abstention, and queue overflow

| Condition | Behavior | Operator action |
| --- | --- | --- |
| `MAX_OPEN` (3) open generated drafts already exist | `prepare` reports `publishable=false` with the reason in the run summary | Merge or close existing drafts, or wait |
| `DAILY_LIMIT` (3, UTC) generated drafts already created today | Same as above | Wait for UTC rollover, or replay tomorrow |
| An open or closed PR already exists for the exact source key | Left untouched; normal replay refuses to recreate it | Review the existing PR; closing it deliberately opts this source out of recreation |
| Another open generated PR targets the same feature page | Abstention | Review that draft before replaying the new source |
| Inference budget exceeded (`MAX_INFERENCE_FILES` / `MAX_EVIDENCE_TEXT_BYTES` / `MAX_TEXT_BYTES`) | Abstention with a replay instruction in the run summary; never a silent truncation | Investigate whether the PR is unusually large; replay is unlikely to change the outcome without raising a cap in a reviewed code change |
| GitHub Actions `queue: max` 100-pending-run cap reached | The excess run is dropped by GitHub, not queued | Identify the dropped `source_pr` and use the explicit publish replay in §5 |
| Model returns malformed/non-schema JSON | Abstention (`ContractError`), never a traceback | If this recurs, investigate the gateway/model configuration |

## 7. Snapshot drift

`publish` re-reads `main`'s tip immediately before rendering. If it no
longer equals the `snapshot_sha` captured at bootstrap, the job **aborts**
— it does not silently re-collect against the new tip and does not retry
automatically, because the evidence and rendered page were computed
against the old snapshot. The run summary directs:

```sh
gh workflow run feature-docs-draft.yml --ref main -f source_pr=N -F dry_run=false
```

This is a full fresh run against the new `main` tip, not a patch of the
aborted one.

## 8. Push/PR-create recovery (operator-only, no re-inference)

If `publish` pushes the deterministic branch but fails before `gh pr
create` returns (for example a transient API error), the branch exists
remotely but no PR does. Do not replay the workflow to fix this — a fresh
replay abstains on the existing branch/source-key collision. Instead:

1. Locate the run's `feature-docs-payload-<run_id>` artifact — the
   canonical `payload.json` uploaded by `prepare` *before* any push. This
   is the only allowed input to recovery.
2. Run the operator-only recovery command against that saved payload:
   ```sh
   python3 etc/scripts/ci/feature_docs.py recover \
     --payload payload.json --snapshot-sha <snapshot_sha>
   ```
3. `recover` fetches the pushed branch, verifies it is based on the
   payload's snapshot, verifies `main` has not drifted from that snapshot,
   verifies the branch changes are exactly the payload's saved
   `changed_paths` (a nonempty subset of the two allowed paths), verifies
   the pushed tree's hash matches `payload_tree_sha256`, and verifies the
   branch tip is app-authored. Only then does it open the draft PR. It
   never calls the model and never regenerates content.
4. If any verification fails — missing payload, drifted `main`, unexpected
   paths, a content mismatch, or an uncertain author — recovery stops with
   a clear error instead of force-pushing or silently recreating the
   branch. Resolve the underlying issue (for example, a genuinely
   corrupted branch) manually; do not bypass recovery's checks.

## 9. Kill switch — what it does and does not do

1. Setting `FEATURE_DOCS_AUTOMATION_ENABLED=false` stops new automatic
   launches at the `bootstrap` job and stops the App-token-mint and
   remote-write steps in any subsequent `publish` job. Read-only dry-run
   preparation remains available while disabled — this is intentional,
   not a leak.
2. For an urgent stop, **disable the `feature-docs-draft.yml` workflow and
   cancel any in-flight runs.** This prevents new runs and stops
   queued/running writes at the next step boundary.
3. This is **not** atomic revocation of an already-issued App installation
   token. A run whose `publish` job has already minted its token may
   complete that single push/PR-create before cancellation takes effect.
   The token is short-lived and scoped to `base/base` with only
   `contents`/`pull-requests`/`issues` write, and the workflow's design —
   never calling a merge API — is what prevents an in-flight run from
   merging anything, not the kill switch itself.
4. There is no live, mid-run `vars` re-check available on the `actions`
   permission model used here; this plan does not claim one. Disable +
   cancel is the whole contract.

## 10. Provenance and review expectations

Every generated draft PR body includes the source PR URL, its merge SHA,
the captured snapshot SHA, the generated page path, its citations, and an
explicit statement that any shown commands are proposed and unverified and
that the bot never merges. `CONTRIBUTING.md` carries a narrow, exact-scope
exception: PRs opened by this specific App identity skip the
assigned-issue requirement, but still require ordinary human review and
are never auto-merged — this exception does not extend to any other bot or
contributor.

## 11. Residual risks

* GitHub's 100-pending-run queue limit can silently drop a run; only a
  manual replay recovers it.
* Large or unusual source PRs can exceed the API pagination ceiling or the
  inference budget and abstain; this is intentional (fail closed on
  incomplete evidence), not a bug to patch by raising a cap without
  review.
* A gateway protocol mismatch blocks inference until its smoke test (§3)
  is re-verified against the live gateway.
* The `infer` job's artifact-endpoint allowlist is runner-specific; an
  unverified allowlist can fail artifact download/upload under blocked
  egress.
* The kill switch is disable-and-cancel, not atomic App-token revocation
  (§9).
* Same-page collision detection is an imperfect heuristic, not exact
  semantic deduplication; a human reviewer remains the final check for
  duplicates and correctness.
* Generated instructions and commands are never executed by the
  automation and remain unverified until a human runs them.
