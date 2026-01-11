#!/usr/bin/env bash
# publish.sh - Publish a release from the latest RC
#
# Usage: ./publish.sh <release_branch> [--prerelease]
# Example: ./publish.sh releases/v1.0.0
#          ./publish.sh releases/v1.0.0 --prerelease
#
# Environment variables:
#   REGISTRY_IMAGE - Docker registry image (default: ghcr.io/base/node-reth-dev)
#   GH_TOKEN       - GitHub token for creating releases
#   DRY_RUN        - Set to "true" to skip actual publish

set -euo pipefail

RELEASE_BRANCH="${1:-}"
PRERELEASE="${2:-}"

if [[ -z "$RELEASE_BRANCH" ]]; then
    echo "Usage: $0 <release_branch> [--prerelease]"
    echo "Example: $0 releases/v1.0.0"
    exit 1
fi

REGISTRY_IMAGE="${REGISTRY_IMAGE:-ghcr.io/base/node-reth-dev}"

# Parse version from branch name
parse_branch_version() {
    local branch="$1"
    if [[ "$branch" =~ ^releases/v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
        echo "${BASH_REMATCH[1]}"
    else
        echo "Error: Invalid branch format: $branch" >&2
        echo "Expected: releases/v<major>.<minor>.<patch>" >&2
        return 1
    fi
}

# Find the latest RC tag for a version
find_latest_rc() {
    local version="$1"
    git tag -l "v${version}-rc.*" | sort -V | tail -1
}

# Verify Docker image exists
verify_docker_image() {
    local tag="$1"
    echo "Checking for image: ${REGISTRY_IMAGE}:${tag}"
    if ! docker manifest inspect "${REGISTRY_IMAGE}:${tag}" > /dev/null 2>&1; then
        echo "Error: RC image not found: ${REGISTRY_IMAGE}:${tag}" >&2
        return 1
    fi
    echo "RC image verified"
}

# Retag Docker image with release tags
retag_docker() {
    local version="$1"
    local rc_tag="$2"
    local release_tag="v${version}"

    # Parse version components
    IFS='.' read -r major minor patch <<< "$version"

    echo "Retagging ${rc_tag} as release tags..."

    docker buildx imagetools create \
        -t "${REGISTRY_IMAGE}:${release_tag}" \
        -t "${REGISTRY_IMAGE}:${major}.${minor}" \
        -t "${REGISTRY_IMAGE}:${major}" \
        -t "${REGISTRY_IMAGE}:latest" \
        "${REGISTRY_IMAGE}:${rc_tag}"

    echo "Docker images published:"
    echo "  - ${REGISTRY_IMAGE}:${release_tag}"
    echo "  - ${REGISTRY_IMAGE}:${major}.${minor}"
    echo "  - ${REGISTRY_IMAGE}:${major}"
    echo "  - ${REGISTRY_IMAGE}:latest"
}

# Generate changelog from commits
generate_changelog() {
    local version="$1"
    local release_tag="v${version}"
    local changelog_file="${2:-changelog.md}"

    # Find previous release tag (exclude RC tags)
    local prev_tag
    prev_tag=$(git tag -l 'v*' | grep -v '\-rc\.' | sort -V | grep -B1 "^${release_tag}$" | head -1 || true)

    # If no previous tag found or it's the same as current
    if [[ -z "$prev_tag" || "$prev_tag" == "$release_tag" ]]; then
        prev_tag=$(git tag -l 'v*' | grep -v '\-rc\.' | sort -V | tail -2 | head -1 || true)
    fi

    local commits compare_url=""
    if [[ -z "$prev_tag" || "$prev_tag" == "$release_tag" ]]; then
        echo "First release - including all commits"
        commits=$(git log --oneline --no-merges HEAD)
    else
        echo "Previous release: $prev_tag"
        commits=$(git log --oneline --no-merges "${prev_tag}..HEAD")
        compare_url="https://github.com/${GITHUB_REPOSITORY:-owner/repo}/compare/${prev_tag}...${release_tag}"
    fi

    # Generate changelog
    cat > "$changelog_file" << 'EOF'
## What's Changed

EOF

    # Features
    echo "### Features" >> "$changelog_file"
    local features
    features=$(echo "$commits" | grep -iE "^[a-f0-9]+ feat" | sed 's/^[a-f0-9]* /- /' || true)
    if [[ -n "$features" ]]; then
        echo "$features" >> "$changelog_file"
    else
        echo "_No new features_" >> "$changelog_file"
    fi
    echo "" >> "$changelog_file"

    # Bug Fixes
    echo "### Bug Fixes" >> "$changelog_file"
    local fixes
    fixes=$(echo "$commits" | grep -iE "^[a-f0-9]+ fix" | sed 's/^[a-f0-9]* /- /' || true)
    if [[ -n "$fixes" ]]; then
        echo "$fixes" >> "$changelog_file"
    else
        echo "_No bug fixes_" >> "$changelog_file"
    fi
    echo "" >> "$changelog_file"

    # Other Changes
    echo "### Other Changes" >> "$changelog_file"
    local other
    other=$(echo "$commits" | grep -viE "^[a-f0-9]+ (feat|fix)" | sed 's/^[a-f0-9]* /- /' || true)
    if [[ -n "$other" ]]; then
        echo "$other" >> "$changelog_file"
    else
        echo "_No other changes_" >> "$changelog_file"
    fi
    echo "" >> "$changelog_file"

    # Docker section
    cat >> "$changelog_file" << EOF

## Docker Images

\`\`\`bash
docker pull ${REGISTRY_IMAGE}:v${version}
docker pull ${REGISTRY_IMAGE}:latest
\`\`\`
EOF

    # Compare link
    if [[ -n "$compare_url" ]]; then
        cat >> "$changelog_file" << EOF

## Full Changelog

[Compare with previous release](${compare_url})
EOF
    fi

    echo "Changelog generated: $changelog_file"
}

# Create GitHub release
create_github_release() {
    local release_tag="$1"
    local changelog_file="$2"
    local prerelease_flag=""

    if [[ "$PRERELEASE" == "--prerelease" ]]; then
        prerelease_flag="--prerelease"
    fi

    gh release create "$release_tag" \
        --title "$release_tag" \
        --notes-file "$changelog_file" \
        $prerelease_flag
}

# Main logic
main() {
    echo "=== Release Publish ==="
    echo "Branch: $RELEASE_BRANCH"

    # Parse version
    VERSION=$(parse_branch_version "$RELEASE_BRANCH")
    RELEASE_TAG="v${VERSION}"
    echo "Version: $VERSION"
    echo "Release tag: $RELEASE_TAG"

    # Find latest RC
    RC_TAG=$(find_latest_rc "$VERSION")
    if [[ -z "$RC_TAG" ]]; then
        echo "Error: No RC tags found for version $VERSION"
        echo "Build at least one RC before publishing"
        exit 1
    fi
    echo "Latest RC: $RC_TAG"

    # Check if release tag already exists
    if git rev-parse "$RELEASE_TAG" >/dev/null 2>&1; then
        echo "Error: Release tag $RELEASE_TAG already exists"
        exit 1
    fi

    # Verify RC image exists (skip in dry run)
    if [[ "${DRY_RUN:-}" != "true" ]]; then
        verify_docker_image "$RC_TAG"
    fi

    if [[ "${DRY_RUN:-}" == "true" ]]; then
        # In dry run, still test changelog generation
        generate_changelog "$VERSION" "changelog.md"
        echo ""
        echo "=== Generated Changelog ==="
        cat changelog.md
        rm -f changelog.md
        echo "[DRY RUN] Would publish release $RELEASE_TAG from $RC_TAG"
        exit 0
    fi

    # Retag Docker images
    retag_docker "$VERSION" "$RC_TAG"

    # Configure git
    git config user.name "github-actions[bot]"
    git config user.email "github-actions[bot]@users.noreply.github.com"

    # Create and push release tag
    git tag -a "$RELEASE_TAG" -m "Release $RELEASE_TAG"
    git push origin "$RELEASE_TAG"

    # Generate changelog
    generate_changelog "$VERSION" "changelog.md"

    # Create GitHub release
    create_github_release "$RELEASE_TAG" "changelog.md"

    echo ""
    echo "=== Release $RELEASE_TAG Published Successfully ==="
    echo "Promoted from: $RC_TAG"
}

main
