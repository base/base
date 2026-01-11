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

# Main logic
main() {
    echo "=== Create Release Tag ==="
    echo "Branch: $RELEASE_BRANCH"
    echo "Type: $RELEASE_TYPE"

    # Parse version from branch name
    VERSION=$(parse_branch_version "$RELEASE_BRANCH")
    echo "Version: $VERSION"

    local TAG

    if [[ "$RELEASE_TYPE" == "rc" ]]; then
        # Determine next RC number
        RC_NUMBER=$(get_next_rc_number "$VERSION")
        TAG="v${VERSION}-rc.${RC_NUMBER}"
        echo "Creating RC tag: $TAG"
    else
        # Final release
        TAG="v${VERSION}"

        # Verify at least one RC exists
        if [[ -z "$(has_rc "$VERSION")" ]]; then
            echo "Error: No RC tags found for version $VERSION"
            echo "Create at least one RC before publishing a final release"
            exit 1
        fi

        echo "Creating final release tag: $TAG"
    fi

    # Verify tag doesn't already exist
    if tag_exists "$TAG"; then
        echo "Error: Release tag $TAG already exists"
        exit 1
    fi

    # Configure git and create tag
    configure_git
    create_and_push_tag "$TAG"

    echo ""
    echo "=== Tag $TAG created and pushed ==="

    # Write tag to file for GHA summary (if RUNNER_TEMP is set)
    if [[ -n "${RUNNER_TEMP:-}" ]]; then
        echo "$TAG" > "${RUNNER_TEMP}/release_tag"
    fi
}

main
