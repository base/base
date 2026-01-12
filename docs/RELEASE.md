# Release Process

## Overview

We use release branches and manual tag creation to build and test release candidates (RCs) before publishing final releases.

## How it works

### 1. Create a release branch

Create a branch with the naming convention `releases/v<version>`:

- **Major/minor releases**: branch off `main`
- **Patch releases**: branch off an existing release branch

When you push the branch, a workflow automatically:
- Creates a PR to update all `Cargo.toml` versions to match the branch version
- Merge the PR before creating releases

### 2. Create release candidates

Run the **Create Release** workflow manually:
- Select your release branch (e.g., `releases/v1.0.0`)
- Choose `rc` as the release type

This will:
- Automatically determine the next RC number (rc.1, rc.2, etc.)
- Create and push a tag (e.g., `v1.0.0-rc.1`)
- Trigger the Release workflow

### 3. Release workflow (automatic)

When a tag is pushed, the Release workflow automatically:
- Runs CI checks (build, test, docker)
- Builds multi-arch Docker images (amd64 + arm64)
- Publishes to GHCR with appropriate tags
- Creates a GitHub release with auto-generated changelog

If you need fixes, merge PRs into the release branch and create another RC.

### 4. Publish final release

Once you're happy with an RC, run the **Create Release** workflow again:
- Select your release branch
- Choose `final` as the release type

This will:
- Create the final version tag (e.g., `v1.0.0`)
- Trigger the Release workflow
- Docker images are tagged as `v1.0.0`, `1.0`, `1`, and `latest`

## Quick reference

| Action | How to trigger |
|--------|----------------|
| Version sync | Automatic on branch creation (`releases/v*`) |
| RC build | Run "Create Release" workflow with `rc` type |
| Final release | Run "Create Release" workflow with `final` type |

## Workflows

- **Release Version Sync** - Creates PR to update Cargo.toml on release branch creation
- **Create Release** - Manual workflow to create RC or final tags
- **Release** - Triggered on tag push, builds Docker and creates GitHub release
