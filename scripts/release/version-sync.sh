#!/usr/bin/env bash
# version-sync.sh - Create a PR to sync Cargo.toml version with release branch name
#
# Usage: ./version-sync.sh <branch_name>
# Example: ./version-sync.sh releases/v1.0.0
#
# Environment variables:
#   GH_TOKEN - GitHub token for creating PRs (required in CI)
#   DRY_RUN  - Set to "true" to skip git push and PR creation

set -euo pipefail

BRANCH_NAME="${1:-}"

if [[ -z "$BRANCH_NAME" ]]; then
    echo "Usage: $0 <branch_name>"
    echo "Example: $0 releases/v1.0.0"
    exit 1
fi

# Parse version from branch name
parse_branch_version() {
    local branch="$1"
    if [[ "$branch" =~ ^releases/v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
        echo "${BASH_REMATCH[1]}"
    else
        echo "Error: Invalid branch name format: $branch" >&2
        echo "Expected: releases/v<major>.<minor>.<patch>" >&2
        return 1
    fi
}

# Get current version from Cargo.toml
get_cargo_version() {
    cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version'
}

# Check if a sync PR already exists
check_existing_pr() {
    local sync_branch="$1"
    gh pr list --head "$sync_branch" --state open --json number --jq '.[0].number // empty' 2>/dev/null || true
}

# Main logic
main() {
    echo "=== Release Version Sync ==="
    echo "Branch: $BRANCH_NAME"

    # Parse version from branch name
    BRANCH_VERSION=$(parse_branch_version "$BRANCH_NAME")
    echo "Target version: $BRANCH_VERSION"

    # Get current Cargo.toml version
    CURRENT_VERSION=$(get_cargo_version)
    echo "Current Cargo.toml version: $CURRENT_VERSION"

    # Check if sync is needed
    if [[ "$CURRENT_VERSION" == "$BRANCH_VERSION" ]]; then
        echo "Version already matches. No sync needed."
        exit 0
    fi

    echo "Version mismatch detected. Creating sync PR..."

    SYNC_BRANCH="release-version-sync/v${BRANCH_VERSION}"

    # Check for existing PR
    EXISTING_PR=$(check_existing_pr "$SYNC_BRANCH")
    if [[ -n "$EXISTING_PR" ]]; then
        echo "Sync PR #$EXISTING_PR already exists. Skipping."
        exit 0
    fi

    # Configure git
    git config user.name "github-actions[bot]"
    git config user.email "github-actions[bot]@users.noreply.github.com"

    # Create sync branch
    git checkout -b "$SYNC_BRANCH"

    # Update version in Cargo.toml (use compatible sed syntax for macOS/Linux)
    if [[ "$OSTYPE" == "darwin"* ]]; then
        sed -i '' "s/^version = \".*\"/version = \"$BRANCH_VERSION\"/" Cargo.toml
    else
        sed -i "s/^version = \".*\"/version = \"$BRANCH_VERSION\"/" Cargo.toml
    fi

    # Update Cargo.lock
    cargo update --workspace

    # Commit changes
    git add Cargo.toml Cargo.lock
    git commit -m "chore(release): set version to $BRANCH_VERSION"

    if [[ "${DRY_RUN:-}" == "true" ]]; then
        echo "[DRY RUN] Would push branch and create PR"
        exit 0
    fi

    # Push sync branch
    git push origin "$SYNC_BRANCH"

    # Create PR
    gh pr create \
        --base "$BRANCH_NAME" \
        --head "$SYNC_BRANCH" \
        --title "chore(release): set version to $BRANCH_VERSION" \
        --body "$(cat <<EOF
## Summary

Auto-generated PR to sync Cargo.toml version with release branch.

- **Release branch**: \`$BRANCH_NAME\`
- **Target version**: \`$BRANCH_VERSION\`

Once merged, the Release RC workflow will automatically build a release candidate Docker image.
EOF
)"

    echo "Version sync PR created successfully."
}

main
