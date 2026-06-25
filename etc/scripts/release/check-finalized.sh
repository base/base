#!/usr/bin/env bash
# check-finalized.sh - Fail if a release branch already has a final release tag
#
# Usage: ./check-finalized.sh <release_branch>
# Example: ./check-finalized.sh releases/v1.0.0

# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RELEASE_BRANCH="${1:-}"

if [[ -z "$RELEASE_BRANCH" ]]; then
    echo "Usage: $0 <release_branch>" >&2
    echo "Example: $0 releases/v1.0.0" >&2
    exit 1
fi

emit_error() {
    local message="$1"

    if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
        echo "::error::$message" >&2
    else
        echo "Error: $message" >&2
    fi
}

main() {
    local version
    if ! version=$(parse_branch_version "$RELEASE_BRANCH"); then
        emit_error "Invalid release branch: $RELEASE_BRANCH (expected releases/v<major>.<minor>.<patch>)"
        exit 1
    fi

    local final_tag="v${version}"
    if tag_exists "$final_tag"; then
        emit_error "Final release tag $final_tag already exists for $RELEASE_BRANCH"
        exit 1
    fi

    echo "No final release tag $final_tag found for $RELEASE_BRANCH."
}

main
