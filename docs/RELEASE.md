# Release Process

## Overview

We use release branches to build and test release candidates (RCs) before publishing final releases.

## How it works

### 1. Create a release branch

Create a branch with the naming convention `releases/v<version>`:

- **Major/minor releases**: branch off `main`
- **Patch releases**: branch off an existing release branch

When you push the branch, a workflow automatically:
- Updates all `Cargo.toml` versions to match the branch version
- Commits and pushes the version bump

### 2. RC builds (automatic)

Every push to the release branch triggers an RC build:
- Builds multi-arch Docker images (amd64 + arm64)
- Tags the image as `v1.0.0-rc.1`, `v1.0.0-rc.2`, etc.
- Creates a matching git tag

If you need fixes, just merge PRs into the release branch. Each merge bumps the RC number.

### 3. Publish the release

Once you're happy with an RC, run the **Release Publish** workflow manually with the release branch name.

This will:
- Re-tag the latest RC image as the final version (e.g., `v1.0.0`)
- Create a GitHub release with auto-generated notes
- Tag the commit as `v1.0.0`

## Quick reference

| Action | Trigger |
|--------|---------|
| Version sync | Branch creation (`releases/v*`) |
| RC build | Any push to `releases/v*` |
| Final release | Manual workflow dispatch |
