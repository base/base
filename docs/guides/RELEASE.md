# Release Process

## Overview

Releases are managed through two manual workflows and one automatic trigger:

- **Start Release** — pick a bump type (major/minor/patch) and the release branch is created automatically
- **Publish Release** — pick a version and the final tag + Docker images + binaries are published
- **Auto-RC** — every commit to a `releases/v*` branch (once the version sync PR is merged) automatically creates an RC tag and builds Docker images + binaries

## Step-by-step

### 1. Start a release

Run the **Start Release** workflow (`Actions → Start Release → Run workflow`):

- Select the bump type: `minor` (new feature release), `patch` (bug fixes), or `major` (breaking changes)
- The workflow computes the next version from the latest final tag and creates the `releases/vX.Y.Z` branch
- For `patch` bumps, the base is the latest existing `releases/vX.Y.*` branch; for `major`/`minor`, the base is `main`

After the branch is created, the **Release Version Sync** workflow fires automatically and opens a PR to update `Cargo.toml` to the new version.

### 2. Merge the version sync PR

Review and merge the auto-generated version sync PR targeting the release branch. This unblocks auto-RC creation.

### 3. Build release candidates (automatic)

Every commit pushed to the release branch triggers the **Create RC** workflow, which:

- Skips silently if `Cargo.toml` is still `0.0.0` (version sync PR not yet merged)
- Otherwise creates the next RC tag (e.g., `v0.6.0-rc.1`, `v0.6.0-rc.2`, …) at the triggering commit
- Dispatches the independent **Build RC** workflow at that tag, then finishes without waiting for artifacts

**Build RC** builds multi-arch Docker images and native binaries using **Build Release**. Docker images are tagged with the RC tag only (not `latest`). Each RC builds in its own workflow run, so newer merges do not cancel or wait for older artifact builds (subject to runner availability).

To create additional RCs, simply push more commits (bug fixes, backports) to the release branch.

### 4. Publish the final release

Once you are satisfied with an RC, run the **Publish Release** workflow (`Actions → Publish Release → Run workflow`):

- Enter the version number (e.g., `0.6.0` — no `v` prefix, no `releases/` prefix)
- The workflow validates that the release branch exists and `Cargo.toml` is not `0.0.0`
- Creates the final tag `vX.Y.Z` on the release branch
- Builds the `base` image once (`PROFILE=maxperf`) and tags it as `vX.Y.Z`, `X.Y`, `X`, and `latest` on `ghcr.io/base/node`
- Creates a draft GitHub release with auto-generated changelog and uploads binaries
- Review and publish the draft release on GitHub

## Auto-RC behavior

The **Create RC** workflow triggers on every push to any `releases/v*` branch. It is safe to push before the version sync PR is merged — it detects the `0.0.0` version and skips with a notice rather than failing.

RC tags follow the pattern `vX.Y.Z-rc.N` where `N` increments automatically based on existing tags.

Only tag allocation and build dispatch are serialized per release branch. The tagging job uses [`queue: max`](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency#example-queueing-multiple-pending-runs), so up to 100 pending jobs wait rather than replace one another. `cancel-in-progress: false` alone does not preserve pending jobs. GitHub orders the queue by when jobs start waiting, not necessarily commit order; each job still tags its own triggering commit. A full queue cancels additional jobs.

The build is explicitly dispatched using `GITHUB_TOKEN`, because tag pushes made with that token [do not trigger other workflows](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow#triggering-a-workflow-from-a-workflow). **Create RC** needs `actions: write` for dispatch, but artifact publishing permissions stay in **Build RC**.

### Retrying RC builds

If tag creation succeeds but dispatch fails, rerun **Create RC**: it reuses the RC tag at that commit. If an artifact build fails, rerun that **Build RC** run, or dispatch the existing tag directly:

```sh
gh workflow run build-rc.yml --ref v1.4.0-rc.1
```

**Build RC** accepts only RC tag refs, not release branches or final tags. Manually dispatching the same tag again starts another build; it does not cancel an existing build. The **Create RC** result covers tagging and dispatch only; check **Build RC** for artifact success.

### Rolling out workflow changes

Merge the new **Build RC** workflow into the default branch first: GitHub requires dispatched workflows to exist there. Then backport the workflow and release-script changes to each active release branch. Dispatch runs at the RC tag, so that commit must also contain `build-rc.yml` and the reusable build workflow. Tags predating this split do not support this dispatch command; retry their original build jobs instead.

Updating only `main` does not change push-triggered workflows on existing release branches. Existing queued or running workflows also retain their original configuration; this change does not retroactively fix cancelled runs or release their concurrency locks.

Before retrying a legacy **Create RC** run, check its checkout ref: older definitions checked out the moving release branch rather than the triggering SHA, so rerunning them can tag a newer commit instead of recovering the missed RC.

## Quick reference

| Action | Workflow | Trigger | Output |
|--------|----------|---------|--------|
| Create release branch | **Start Release** | Manual (bump type) | `releases/vX.Y.Z` branch |
| Sync Cargo.toml version | **Release Version Sync** | Automatic on branch creation | PR targeting release branch |
| Create RC tag | **Create RC** | Automatic on push to `releases/v*` | RC tag + independent build dispatch |
| Build RC artifacts | **Build RC** | Dispatched by Create RC at the RC tag | Docker image + binaries |
| Publish final release | **Publish Release** | Manual (version number) | Final tag + Docker images + draft GitHub release |

## Workflows

- **Start Release** — Creates the release branch from a bump-type dropdown
- **Release Version Sync** — Opens a PR to update `Cargo.toml` when a release branch is created
- **Create RC** — Triggered on push to `releases/v*`; creates RC tags and dispatches builds without waiting for artifacts
- **Build RC** — Dispatched at an RC tag; validates the ref and builds artifacts independently of Create RC
- **Build Release** — Reusable workflow (called by Build RC and Publish Release) that builds Docker images and binaries
- **Publish Release** — Manual workflow to create the final tag and publish the release
