#!/usr/bin/env bash
# Post or update the single iai benchmark comment on a PR, identified by its
# marker. The same marker is reused for the in-progress, results, and failure
# states so there is only ever one comment, updated in place.
#
# Usage: post_pr_comment.sh BODY_FILE
# Requires env: GH_TOKEN, REPO (owner/name), PR_NUMBER.
set -euo pipefail

marker='<!-- iai-bench-results -->'
body="$(cat "$1")"

existing_id=$(gh api "repos/${REPO}/issues/${PR_NUMBER}/comments" \
  --jq ".[] | select(.body | startswith(\"${marker}\")) | .id" \
  | head -1)

if [ -n "$existing_id" ]; then
  gh api "repos/${REPO}/issues/comments/${existing_id}" \
    -X PATCH --field body="$body" > /dev/null
else
  gh api "repos/${REPO}/issues/${PR_NUMBER}/comments" \
    --field body="$body" > /dev/null
fi
