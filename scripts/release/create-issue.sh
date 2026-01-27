#!/usr/bin/env bash
# create-issue.sh - Create a release tracking issue on GitHub
#
# Usage: ./create-issue.sh <release_branch>
# Example: ./create-issue.sh releases/v1.0.0

# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RELEASE_BRANCH="${1:-}"

if [[ -z "$RELEASE_BRANCH" ]]; then
    echo "Usage: $0 <release_branch>"
    echo "  release_branch: e.g., releases/v1.0.0"
    exit 1
fi

VERSION=$(parse_branch_version "$RELEASE_BRANCH")

echo "=== Create Release Tracking Issue ==="
echo "Branch: $RELEASE_BRANCH"
echo "Version: $VERSION"

BODY="## Release Checklist for v${VERSION}

- [ ] Create RC
- [ ] Deploy to devnet
- [ ] Deploy to testnet
- [ ] Deploy to mainnet
- [ ] Create final release

---
*This issue was automatically created by the release workflow.*"

ASSIGNEE="${GITHUB_ACTOR:-}"
ASSIGNEE_FLAG=""
if [[ -n "$ASSIGNEE" ]]; then
    ASSIGNEE_FLAG="--assignee $ASSIGNEE"
fi

ISSUE_URL=$(gh issue create \
    --title "chore(release): create v${VERSION} release" \
    --body "$BODY" \
    $ASSIGNEE_FLAG)

ISSUE_NUMBER=$(echo "$ISSUE_URL" | grep -oE '[0-9]+$')

echo ""
echo "=== Issue Created ==="
echo "Issue: #${ISSUE_NUMBER}"
echo "URL: ${ISSUE_URL}"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo ""
        echo "## Release Issue Created"
        echo ""
        echo "**Issue**: #${ISSUE_NUMBER}"
        echo "**Assignee**: @${ASSIGNEE:-N/A}"
    } >> "$GITHUB_STEP_SUMMARY"
fi
