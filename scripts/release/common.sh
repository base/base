#!/usr/bin/env bash
# common.sh - Shared helper functions for release scripts
#
# Usage: source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

set -euo pipefail

# Parse version from branch name (e.g., releases/v1.0.0 -> 1.0.0)
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

# Parse version from tag (e.g., v1.0.0 -> 1.0.0, v1.0.0-rc.1 -> 1.0.0)
parse_tag_version() {
    local tag="$1"
    # Remove 'v' prefix and any -rc.N suffix
    echo "$tag" | sed -E 's/^v//; s/-rc\.[0-9]+$//'
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

# Check if any RC exists for a version
has_rc() {
    local version="$1"
    git tag -l "v${version}-rc.*" | head -1
}

# Check if a tag already exists
tag_exists() {
    local tag="$1"
    git rev-parse "$tag" >/dev/null 2>&1
}

# Configure git for automated commits
configure_git() {
    git config user.name "github-actions[bot]"
    git config user.email "github-actions[bot]@users.noreply.github.com"
}

# Create and push a git tag
create_and_push_tag() {
    local tag="$1"
    local message="${2:-Release $tag}"

    git tag -a "$tag" -m "$message"
    git push origin "$tag"
}

# Check if a PR already exists for a branch
check_existing_pr() {
    local head_branch="$1"
    gh pr list --head "$head_branch" --state open --json number --jq '.[0].number // empty' 2>/dev/null || true
}

# Find the previous release tag for changelog generation
# Returns empty string if no previous tag found
find_previous_tag() {
    local current_tag="$1"
    local prev_tag

    # First try: find previous final release tag
    prev_tag=$(git tag -l 'v*' | grep -v '\-rc\.' | sort -V | grep -B1 "^${current_tag}$" | head -1 || true)

    # If no previous tag found or it's the same as current, get second-to-last final release
    if [[ -z "$prev_tag" || "$prev_tag" == "$current_tag" ]]; then
        prev_tag=$(git tag -l 'v*' | grep -v '\-rc\.' | sort -V | tail -2 | head -1 || true)
    fi

    # If still nothing (this is an RC or first release), try any previous tag
    if [[ -z "$prev_tag" || "$prev_tag" == "$current_tag" ]]; then
        prev_tag=$(git tag -l 'v*' | sort -V | grep -B1 "^${current_tag}$" | head -1 || true)
    fi

    # Return empty if we only found the current tag
    if [[ "$prev_tag" == "$current_tag" ]]; then
        echo ""
    else
        echo "$prev_tag"
    fi
}

# Get commits between two tags (or recent commits if no previous tag)
get_commits_since() {
    local prev_tag="$1"
    local limit="${2:-50}"

    if [[ -z "$prev_tag" ]]; then
        git log --oneline --no-merges -"$limit" HEAD
    else
        git log --oneline --no-merges "${prev_tag}..HEAD"
    fi
}
