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
- Otherwise creates the next RC tag (e.g., `v0.6.0-rc.1`, `v0.6.0-rc.2`, …)
- Builds multi-arch Docker images and native binaries
- Pushes the Docker image tagged with the RC tag only (not `latest`)

To create additional RCs, simply push more commits (bug fixes, backports) to the release branch.

### 4. Publish the final release

Once you are satisfied with an RC, run the **Publish Release** workflow (`Actions → Publish Release → Run workflow`):

- Enter the version number (e.g., `0.6.0` — no `v` prefix, no `releases/` prefix)
- The workflow validates that the release branch exists and `Cargo.toml` is not `0.0.0`
- Creates the final tag `vX.Y.Z` on the release branch
- Builds Docker images tagged as `vX.Y.Z`, `X.Y`, `X`, and `latest`
- Creates a draft GitHub release with auto-generated changelog and uploads binaries
- Review and publish the draft release on GitHub

## Auto-RC behavior

The **Create RC** workflow triggers on every push to any `releases/v*` branch. It is safe to push before the version sync PR is merged — it detects the `0.0.0` version and skips with a notice rather than failing.

RC tags follow the pattern `vX.Y.Z-rc.N` where `N` increments automatically based on existing tags.

## Quick reference

| Action | Workflow | Trigger | Output |
|--------|----------|---------|--------|
| Create release branch | **Start Release** | Manual (bump type) | `releases/vX.Y.Z` branch |
| Sync Cargo.toml version | **Release Version Sync** | Automatic on branch creation | PR targeting release branch |
| Build RC | **Create RC** | Automatic on push to `releases/v*` | RC tag + Docker image + binaries |
| Publish final release | **Publish Release** | Manual (version number) | Final tag + Docker images + draft GitHub release |

## Workflows

- **Start Release** — Creates the release branch from a bump-type dropdown
- **Release Version Sync** — Opens a PR to update `Cargo.toml` when a release branch is created
- **Create RC** — Triggered on push to `releases/v*`; creates RC tags and builds artifacts
- **Build Release** — Reusable workflow (called by Create RC and Publish Release) that builds Docker images and binaries
- **Publish Release** — Manual workflow to create the final tag and publish the release
