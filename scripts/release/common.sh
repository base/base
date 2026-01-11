#!/usr/bin/env bash
# common.sh - Shared helper functions for release scripts
#
# Usage: source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

set -euo pipefail

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

get_cargo_version() {
    cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version'
}

get_next_rc_number() {
    local version="$1"
    local latest_rc
    latest_rc=$(git tag -l "v${version}-rc.*" | sed 's/.*-rc\.//' | sort -n | tail -1)

    if [[ -z "$latest_rc" ]]; then
        echo "1"
    else
        echo "$((latest_rc + 1))"
    fi
}

tag_exists() {
    local tag="$1"
    git rev-parse "$tag" >/dev/null 2>&1
}

configure_git() {
    git config user.name "github-actions[bot]"
    git config user.email "github-actions[bot]@users.noreply.github.com"
}

create_and_push_tag() {
    local tag="$1"
    local message="${2:-Release $tag}"

    git tag -a "$tag" -m "$message"
    git push origin "$tag"
}

check_existing_pr() {
    local head_branch="$1"
    gh pr list --head "$head_branch" --state open --json number --jq '.[0].number // empty' 2>/dev/null || true
}
