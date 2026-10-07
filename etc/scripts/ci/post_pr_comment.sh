#!/usr/bin/env bash
# Post or update a single benchmark comment on a PR, identified by its marker.
# The same marker is reused for the in-progress, results, and failure states so
# there is only ever one comment per marker, updated in place. BODY_FILE must
# start with the marker.
#
# Usage: post_pr_comment.sh BODY_FILE [MARKER]
# MARKER defaults to the iai benchmark comment's marker.
# Requires env: GH_TOKEN, REPO (owner/name), PR_NUMBER.
set -euo pipefail

marker="${2:-<!-- iai-bench-results -->}"

existing_id=$(gh api "repos/${REPO}/issues/${PR_NUMBER}/comments" \
  --jq ".[] | select(.body | startswith(\"${marker}\")) | .id" \
  | head -1)

# Read the body straight from the file via gh's `@<path>` syntax so the markdown
# (which contains backtick-wrapped benchmark names) never passes through a shell
# variable at all.
if [ -n "$existing_id" ]; then
  gh api "repos/${REPO}/issues/comments/${existing_id}" \
    -X PATCH -F body=@"$1" > /dev/null
else
  gh api "repos/${REPO}/issues/${PR_NUMBER}/comments" \
    -F body=@"$1" > /dev/null
fi
