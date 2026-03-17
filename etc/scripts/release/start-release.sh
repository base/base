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

    # For major/minor bumps, include release branch names so we don't
    # collide with an in-progress release that hasn't been tagged yet.
    # For patch bumps, use only final tags so we patch the latest
    # *completed* release, not an in-progress one.
    TAG_VERSIONS=$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' \
        | grep -Ex 'v[0-9]+\.[0-9]+\.[0-9]+' || true)

    if [[ "$BUMP_TYPE" == "patch" ]]; then
        CURRENT_VERSION=$(echo "$TAG_VERSIONS" | sort -V | tail -1 | sed 's/^v//')
    else
        BRANCH_VERSIONS=$(git ls-remote --heads origin 'releases/v*' \
            | sed -n 's|.*refs/heads/releases/\(v[0-9]*\.[0-9]*\.[0-9]*\)$|\1|p' || true)
        CURRENT_VERSION=$(printf '%s\n' $TAG_VERSIONS $BRANCH_VERSIONS \
            | sort -Vu | tail -1 | sed 's/^v//')
    fi

    if [[ -z "$CURRENT_VERSION" ]]; then
        echo "Error: No release tags or release branches found. Cannot determine current version." >&2
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
