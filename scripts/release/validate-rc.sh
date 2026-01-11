#!/usr/bin/env bash
# validate-rc.sh - Validate release branch and determine next RC number
#
# Usage: ./validate-rc.sh <branch_name>
# Example: ./validate-rc.sh releases/v1.0.0
#
# Outputs (as KEY=VALUE for GitHub Actions):
#   version      - The parsed version (e.g., 1.0.0)
#   skip_build   - "true" if build should be skipped (version mismatch)
#   rc_number    - The next RC number (e.g., 1, 2, 3)
#   rc_tag       - The full RC tag (e.g., v1.0.0-rc.1)

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

# Find the next RC number based on existing tags
get_next_rc_number() {
    local version="$1"
    local latest_rc

    # Find existing RC tags for this version
    latest_rc=$(git tag -l "v${version}-rc.*" | sed 's/.*-rc\.//' | sort -n | tail -1)

    if [[ -z "$latest_rc" ]]; then
        echo "1"
    else
        echo "$((latest_rc + 1))"
    fi
}

# Output helper for GitHub Actions
output() {
    local key="$1"
    local value="$2"
    echo "$key=$value"
    if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
        echo "$key=$value" >> "$GITHUB_OUTPUT"
    fi
}

# Main logic
main() {
    echo "=== Release RC Validation ==="
    echo "Branch: $BRANCH_NAME"

    # Parse version from branch name
    BRANCH_VERSION=$(parse_branch_version "$BRANCH_NAME")
    echo "Branch version: $BRANCH_VERSION"

    # Get current Cargo.toml version
    CARGO_VERSION=$(get_cargo_version)
    echo "Cargo.toml version: $CARGO_VERSION"

    # Check if versions match
    if [[ "$CARGO_VERSION" != "$BRANCH_VERSION" ]]; then
        echo "Warning: Version mismatch - Cargo.toml has $CARGO_VERSION, branch expects $BRANCH_VERSION"
        echo "Waiting for version sync PR to be merged"
        output "version" "$BRANCH_VERSION"
        output "skip_build" "true"
        exit 0
    fi

    echo "Version matches: $BRANCH_VERSION"

    # Determine next RC number
    RC_NUMBER=$(get_next_rc_number "$BRANCH_VERSION")
    RC_TAG="v${BRANCH_VERSION}-rc.${RC_NUMBER}"

    echo "Next RC: $RC_TAG"

    output "version" "$BRANCH_VERSION"
    output "skip_build" "false"
    output "rc_number" "$RC_NUMBER"
    output "rc_tag" "$RC_TAG"
}

main
