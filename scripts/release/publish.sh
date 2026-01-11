#!/usr/bin/env bash
# publish.sh - Create a GitHub release with changelog
#
# Usage: ./publish.sh <tag> [--prerelease]
# Example: ./publish.sh v1.0.0
#          ./publish.sh v1.0.0-rc.1 --prerelease
#
# Environment variables:
#   REGISTRY_IMAGE - Docker registry image (default: ghcr.io/base/node-reth-dev)
#   GH_TOKEN       - GitHub token for creating releases

# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

TAG="${1:-}"
PRERELEASE="${2:-}"

if [[ -z "$TAG" ]]; then
    echo "Usage: $0 <tag> [--prerelease]"
    echo "Example: $0 v1.0.0"
    exit 1
fi

REGISTRY_IMAGE="${REGISTRY_IMAGE:-ghcr.io/base/node-reth-dev}"

# Generate changelog from commits
generate_changelog() {
    local tag="$1"
    local changelog_file="${2:-changelog.md}"

    local prev_tag
    prev_tag=$(find_previous_tag "$tag")

    local commits compare_url=""
    if [[ -z "$prev_tag" ]]; then
        echo "First release - including recent commits"
        commits=$(get_commits_since "" 50)
    else
        echo "Previous release: $prev_tag"
        commits=$(get_commits_since "$prev_tag")
        compare_url="https://github.com/${GITHUB_REPOSITORY:-owner/repo}/compare/${prev_tag}...${tag}"
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
docker pull ${REGISTRY_IMAGE}:${tag}
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
    local tag="$1"
    local changelog_file="$2"
    local prerelease_flag=""

    if [[ "$PRERELEASE" == "--prerelease" ]]; then
        prerelease_flag="--prerelease"
    fi

    # Check if release already exists
    if gh release view "$tag" >/dev/null 2>&1; then
        echo "Release $tag already exists, updating..."
        gh release edit "$tag" \
            --title "$tag" \
            --notes-file "$changelog_file" \
            $prerelease_flag
    else
        gh release create "$tag" \
            --title "$tag" \
            --notes-file "$changelog_file" \
            $prerelease_flag
    fi
}

# Main logic
main() {
    echo "=== Create GitHub Release ==="
    echo "Tag: $TAG"

    VERSION=$(parse_tag_version "$TAG")
    echo "Version: $VERSION"

    # Generate changelog
    generate_changelog "$TAG" "changelog.md"

    # Create GitHub release
    create_github_release "$TAG" "changelog.md"

    # Cleanup
    rm -f changelog.md

    echo ""
    echo "=== Release $TAG Created Successfully ==="
}

main
