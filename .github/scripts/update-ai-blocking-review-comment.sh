#!/usr/bin/env bash
set -euo pipefail

marker="<!-- CLAUDE_BLOCKING_REVIEW:v1"

required_vars=(GITHUB_REPOSITORY PR_NUMBER HEAD_SHA)
for var in "${required_vars[@]}"; do
  if [[ -z "${!var:-}" ]]; then
    echo "$var is required" >&2
    exit 1
  fi
done

bypass_label="${BYPASS_LABEL:-ai-review-override}"
pr_labels_json="${PR_LABELS_JSON:-[]}"

body_file="$(mktemp)"
trap 'rm -f "$body_file"' EXIT

has_bypass_label() {
  jq -e --arg label "$bypass_label" '
    type == "array" and any(.[]; . == $label)
  ' >/dev/null 2>&1 <<< "$pr_labels_json"
}

if has_bypass_label; then
  marker_payload="$(jq -cn \
    --arg head_sha "$HEAD_SHA" \
    --arg bypass_label "$bypass_label" \
    '{head_sha: $head_sha, bypassed: true, bypass_label: $bypass_label}')"

  {
    printf '%s\n' "$marker"
    printf '%s\n' "$marker_payload"
    printf '%s\n\n' "-->"
    printf '## AI Blocking Review\n\n'
    printf 'AI blocking review enforcement was bypassed for `%s` because the `%s` label is present.\n\n' "$HEAD_SHA" "$bypass_label"
    printf 'Remove the `%s` label to rerun the required AI blocking review gate.\n' "$bypass_label"
  } > "$body_file"
else
  if [[ -z "${REVIEW_JSON:-}" ]]; then
    echo "REVIEW_JSON is required when the bypass label is absent" >&2
    exit 1
  fi

  if ! jq -e . >/dev/null <<< "$REVIEW_JSON"; then
    echo "REVIEW_JSON is not valid JSON" >&2
    exit 1
  fi

  if ! jq -e --arg head_sha "$HEAD_SHA" '
    (.head_sha == $head_sha) and
    (.has_blocking_findings | type == "boolean") and
    (.findings | type == "array") and
    (.max_severity == "NONE" or .max_severity == "HIGH" or .max_severity == "CRITICAL")
  ' >/dev/null <<< "$REVIEW_JSON"; then
    echo "Blocking review result is malformed or stale" >&2
    exit 1
  fi

has_blocking_findings="$(jq -r '.has_blocking_findings' <<< "$REVIEW_JSON")"
max_severity="$(jq -r '.max_severity' <<< "$REVIEW_JSON")"
blocking_count="$(jq '[.findings[] | select(.severity == "HIGH" or .severity == "CRITICAL")] | length' <<< "$REVIEW_JSON")"
summary="$(jq -r '.summary // ""' <<< "$REVIEW_JSON")"

effective_has_blocking_findings="false"
if [[ "$has_blocking_findings" == "true" || "$max_severity" != "NONE" || "$blocking_count" != "0" ]]; then
  effective_has_blocking_findings="true"
fi

marker_payload="$(jq -cn \
  --arg head_sha "$HEAD_SHA" \
  --arg max_severity "$max_severity" \
  --argjson has_blocking_findings "$effective_has_blocking_findings" \
  '{head_sha: $head_sha, has_blocking_findings: $has_blocking_findings, max_severity: $max_severity, bypassed: false}')"

{
  printf '%s\n' "$marker"
  printf '%s\n' "$marker_payload"
  printf '%s\n\n' "-->"
  printf '## AI Blocking Review\n\n'

  if [[ "$effective_has_blocking_findings" == "true" ]]; then
    printf 'Blocking findings were reported for `%s`.\n\n' "$HEAD_SHA"
    if [[ -n "$summary" ]]; then
      printf '%s\n\n' "$summary"
    fi

    rendered_findings="$(jq -r '
      .findings[]
      | select(.severity == "HIGH" or .severity == "CRITICAL")
      | "### \(.severity): \(.title)\n\n" +
        "- Location: `\(.path // "unknown")\(if (.line // null) then ":\(.line)" else "" end)`\n" +
        "- Confidence: `\(.confidence // "unspecified")`\n\n" +
        "**Evidence**\n\n\(.evidence // "Not provided")\n\n" +
        "**Required fix**\n\n\(.required_fix // "Not provided")\n"
    ' <<< "$REVIEW_JSON")"

    if [[ -n "$rendered_findings" ]]; then
      printf '%s\n' "$rendered_findings"
    else
      printf 'The structured review marked this PR as blocking, but did not include a renderable HIGH or CRITICAL finding. Treat this as fail-closed and rerun the review after checking the workflow logs.\n'
    fi
  else
    printf 'No critical or high issues were reported for `%s`.\n\n' "$HEAD_SHA"
    if [[ -n "$summary" ]]; then
      printf '%s\n' "$summary"
    fi
  fi
} > "$body_file"
fi

existing_id="$(gh api "repos/${GITHUB_REPOSITORY}/issues/${PR_NUMBER}/comments" \
  --paginate \
  --jq ".[] | select(.user.login == \"github-actions[bot]\") | select(.body | startswith(\"${marker}\")) | .id" \
  | sed '/^$/d' \
  | sed -n '1p')"

body="$(cat "$body_file")"

if [[ -n "$existing_id" ]]; then
  gh api "repos/${GITHUB_REPOSITORY}/issues/comments/${existing_id}" \
    -X PATCH \
    --raw-field body="$body" > /dev/null
else
  gh api "repos/${GITHUB_REPOSITORY}/issues/${PR_NUMBER}/comments" \
    --raw-field body="$body" > /dev/null
fi
