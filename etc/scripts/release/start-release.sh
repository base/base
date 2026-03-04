#!/usr/bin/env bash
# start-release.sh - Create a new release branch with bumped version
#
# Usage: ./start-release.sh <bump_type>  (major|minor|patch)

set -euo pipefail

# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

BUMP_TYPE="${1:-}"

if [[ -z "$BUMP_TYPE" ]]; then
    echo "Usage: $0 <bump_type>" >&2
    echo "  bump_type: major, minor, or patch" >&2
    exit 1
fi

if [[ "$BUMP_TYPE" != "major" && "$BUMP_TYPE" != "minor" && "$BUMP_TYPE" != "patch" ]]; then
    echo "Error: bump_type must be 'major', 'minor', or 'patch'" >&2
    exit 1
fi

main() {
    echo "=== Start Release (bump: $BUMP_TYPE) ==="

    # Get latest FINAL tag (exclude RCs)
    CURRENT_VERSION=$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' \
        | grep -v -- '-rc\.' | sort -V | tail -1 | sed 's/^v//')

    if [[ -z "$CURRENT_VERSION" ]]; then
        echo "Error: No final release tags found. Cannot determine current version." >&2
        exit 1
    fi

    echo "Current version: $CURRENT_VERSION"

    IFS='.' read -r major minor patch <<< "$CURRENT_VERSION"

    local base
    case "$BUMP_TYPE" in
        major) major=$((major + 1)); minor=0; patch=0; base="main" ;;
        minor) minor=$((minor + 1)); patch=0; base="main" ;;
        patch)
            patch=$((patch + 1))
            # Find latest releases/v{major}.{minor}.* branch on remote as base
            base=$(git ls-remote --heads origin "releases/v${major}.${minor}.*" \
                | awk '{print $2}' | sed 's|refs/heads/||' | sort -V | tail -1)
            if [[ -z "$base" ]]; then
                echo "Error: No existing releases/v${major}.${minor}.* branch found on remote." >&2
                echo "Cannot create a patch release without a prior release branch." >&2
                exit 1
            fi
            ;;
    esac

    NEW_VERSION="${major}.${minor}.${patch}"
    RELEASE_BRANCH="releases/v${NEW_VERSION}"

    echo "New version: $NEW_VERSION"
    echo "Release branch: $RELEASE_BRANCH"
    echo "Base: $base"

    # Error if branch already exists on remote
    if git ls-remote --heads origin "$RELEASE_BRANCH" | grep -q "$RELEASE_BRANCH"; then
        echo "Error: Branch $RELEASE_BRANCH already exists on remote." >&2
        exit 1
    fi

    configure_git
    git fetch origin "$base"
    git checkout -b "$RELEASE_BRANCH" "origin/$base"
    git push origin "$RELEASE_BRANCH"

    # Write outputs for workflow step summary
    if [[ -n "${RUNNER_TEMP:-}" ]]; then
        echo "$CURRENT_VERSION" > "${RUNNER_TEMP}/current_version"
        echo "$NEW_VERSION"     > "${RUNNER_TEMP}/new_version"
        echo "$RELEASE_BRANCH"  > "${RUNNER_TEMP}/release_branch"
    fi

    echo ""
    echo "=== Branch $RELEASE_BRANCH created (v$CURRENT_VERSION → v$NEW_VERSION) ==="
}

main
