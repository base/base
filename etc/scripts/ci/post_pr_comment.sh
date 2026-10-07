#!/usr/bin/env bash
# Post or update the single iai benchmark comment on a PR, identified by its
# marker. The same marker is reused for the in-progress, results, and failure
# states so there is only ever one comment, updated in place.
#
# Usage: post_pr_comment.sh BODY_FILE
# Requires env: GH_TOKEN, REPO (owner/name), PR_NUMBER. Requires gh and jq.
set -euo pipefail

marker='<!-- iai-bench-results -->'

work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
response="$work_dir/response.json"
errors="$work_dir/errors"
comments_endpoint="repos/${REPO}/issues/${PR_NUMBER}/comments"
max_attempts=3

# Fetch raw JSON first: gh's --jq aborts on an empty/truncated response without
# identifying the failed request. Only the read is safe to retry automatically.
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  if gh api "$comments_endpoint" > "$response" 2> "$errors" &&
    jq -se 'length == 1 and (.[0] | type == "array" and all(.[];
      (.id | type == "number") and (.id > 0) and (.id == (.id | floor)) and
      (.body == null or (.body | type == "string"))))' "$response" > /dev/null 2> "$errors"; then
    break
  fi
  echo "Failed to list PR comments from $comments_endpoint: request failed or returned empty, incomplete, or invalid JSON (attempt $attempt/$max_attempts)." >&2
  cat "$errors" >&2
  if ((attempt == max_attempts)); then
    echo 'No comment was written. Check GitHub API connectivity, GH_TOKEN permissions, and the response body before rerunning this step.' >&2
    exit 1
  fi
  sleep "$attempt"
done

existing_id=$(jq -r --arg marker "$marker" \
  'first(.[] | select((.body // "") | startswith($marker)) | .id) // empty' "$response")

# Read the body straight from the file via gh's `@<path>` syntax so the markdown
# (which contains backtick-wrapped benchmark names) never passes through a shell
# variable at all.
if [ -n "$existing_id" ]; then
  write_endpoint="repos/${REPO}/issues/comments/${existing_id}"
  method=PATCH
else
  write_endpoint="$comments_endpoint"
  method=POST
fi

# A write may have succeeded even if its response was lost. Do not retry it
# blindly: a POST retry could create duplicate benchmark comments.
if ! gh api "$write_endpoint" -X "$method" -F body=@"$1" > "$response" 2> "$errors"; then
  echo "Failed to $method benchmark comment at $write_endpoint; the write outcome is unknown." >&2
  cat "$errors" >&2
  echo 'Check the PR for an existing benchmark comment before rerunning this step.' >&2
  exit 1
fi
if ! jq -se 'length == 1 and (.[0] | type == "object" and
  (.id | type == "number") and (.id > 0) and (.id == (.id | floor)))' \
  "$response" > /dev/null 2> "$errors"; then
  echo "GitHub returned empty, incomplete, or invalid JSON after $method $write_endpoint; the comment may have been written." >&2
  cat "$errors" >&2
  echo 'Check the PR for an existing benchmark comment before rerunning this step.' >&2
  exit 1
fi
