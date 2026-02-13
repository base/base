#!/usr/bin/env bash
# create-tag.sh - Create and push a release tag (RC or final)
#
# Usage: ./create-tag.sh <release_branch> <release_type>
# Example: ./create-tag.sh releases/v1.0.0 rc
#          ./create-tag.sh releases/v1.0.0 final

# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RELEASE_BRANCH="${1:-}"
RELEASE_TYPE="${2:-}"

if [[ -z "$RELEASE_BRANCH" || -z "$RELEASE_TYPE" ]]; then
    echo "Usage: $0 <release_branch> <release_type>"
    echo "  release_branch: e.g., releases/v1.0.0"
    echo "  release_type: rc or final"
    exit 1
fi

if [[ "$RELEASE_TYPE" != "rc" && "$RELEASE_TYPE" != "final" ]]; then
    echo "Error: release_type must be 'rc' or 'final'"
    exit 1
fi

main() {
    echo "=== Create Release Tag ==="
    echo "Branch: $RELEASE_BRANCH"
    echo "Type: $RELEASE_TYPE"

    VERSION=$(parse_branch_version "$RELEASE_BRANCH")
    echo "Version: $VERSION"

    local TAG

    if [[ "$RELEASE_TYPE" == "rc" ]]; then
        RC_NUMBER=$(get_next_rc_number "$VERSION")
        TAG="v${VERSION}-rc.${RC_NUMBER}"
        echo "Creating RC tag: $TAG"
    else
        TAG="v${VERSION}"
        echo "Creating final release tag: $TAG"
    fi

    if tag_exists "$TAG"; then
        echo "Error: Release tag $TAG already exists"
        exit 1
    fi

    configure_git
    create_and_push_tag "$TAG"

    echo ""
    echo "=== Tag $TAG created and pushed ==="

    if [[ -n "${RUNNER_TEMP:-}" ]]; then
        echo "$TAG" > "${RUNNER_TEMP}/release_tag"
    fi
}

main
