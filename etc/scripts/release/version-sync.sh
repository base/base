#!/usr/bin/env bash
# version-sync.sh - Create a PR to sync Cargo.toml version with release branch name
#
# Usage: ./version-sync.sh <branch_name>
# Example: ./version-sync.sh releases/v1.0.0
#
# Environment variables:
#   GH_TOKEN - GitHub token for creating PRs (required in CI)
#   DRY_RUN  - Set to "true" to skip git push and PR creation

# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

BRANCH_NAME="${1:-}"

if [[ -z "$BRANCH_NAME" ]]; then
    echo "Usage: $0 <branch_name>"
    echo "Example: $0 releases/v1.0.0"
    exit 1
fi

main() {
    echo "=== Release Version Sync ==="
    echo "Branch: $BRANCH_NAME"

    BRANCH_VERSION=$(parse_branch_version "$BRANCH_NAME")
    echo "Target version: $BRANCH_VERSION"

    CURRENT_VERSION=$(get_cargo_version)
    echo "Current Cargo.toml version: $CURRENT_VERSION"

    if [[ "$CURRENT_VERSION" == "$BRANCH_VERSION" ]]; then
        echo "Version already matches. No sync needed."
        exit 0
    fi

    echo "Version mismatch detected. Creating sync PR..."

    SYNC_BRANCH="release-version-sync/v${BRANCH_VERSION}"

    EXISTING_PR=$(check_existing_pr "$SYNC_BRANCH")
    if [[ -n "$EXISTING_PR" ]]; then
        echo "Sync PR #$EXISTING_PR already exists. Skipping."
        exit 0
    fi

    configure_git
    git checkout -b "$SYNC_BRANCH"

    # Use compatible sed syntax for macOS/Linux
    if [[ "$OSTYPE" == "darwin"* ]]; then
        sed -i '' "s/^version = \".*\"/version = \"$BRANCH_VERSION\"/" Cargo.toml
    else
        sed -i "s/^version = \".*\"/version = \"$BRANCH_VERSION\"/" Cargo.toml
    fi

    cargo update --workspace
    git add Cargo.toml Cargo.lock
    git commit -m "chore(release): set version to $BRANCH_VERSION"

    if [[ "${DRY_RUN:-}" == "true" ]]; then
        echo "[DRY RUN] Would push branch and create PR"
        exit 0
    fi

    git push origin "$SYNC_BRANCH"

    gh pr create \
        --base "$BRANCH_NAME" \
        --head "$SYNC_BRANCH" \
        --title "chore(release): set version to $BRANCH_VERSION" \
        --body "$(cat <<EOF
## Summary

Auto-generated PR to sync Cargo.toml version with release branch.

- **Release branch**: \`$BRANCH_NAME\`
- **Target version**: \`$BRANCH_VERSION\`

Once merged, run the **Create Release** workflow to build a release candidate.
EOF
)"

    echo "Version sync PR created successfully."
}

main
