#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${HEAD_SHA:-}" ]]; then
  echo "HEAD_SHA is required" >&2
  exit 1
fi

if [[ -z "${REVIEW_JSON:-}" ]]; then
  echo "REVIEW_JSON is required" >&2
  exit 1
fi

summary_file="${GITHUB_STEP_SUMMARY:-}"

write_summary() {
  if [[ -n "$summary_file" ]]; then
    printf '%s\n' "$@" >> "$summary_file"
  fi
}

if ! jq -e . >/dev/null <<< "$REVIEW_JSON"; then
  write_summary "## AI Blocking Review" "" "The blocking review did not return valid JSON."
  echo "REVIEW_JSON is not valid JSON" >&2
  exit 1
fi

if ! jq -e --arg head_sha "$HEAD_SHA" '
  (.head_sha == $head_sha) and
  (.has_blocking_findings | type == "boolean") and
  (.findings | type == "array") and
  (.max_severity == "NONE" or .max_severity == "HIGH" or .max_severity == "CRITICAL")
' >/dev/null <<< "$REVIEW_JSON"; then
  write_summary \
    "## AI Blocking Review" \
    "" \
    "The blocking review result is malformed or does not match the current PR head SHA." \
    "" \
    "- Expected head SHA: \`$HEAD_SHA\`" \
    "- Reported head SHA: \`$(jq -r '.head_sha // "missing"' <<< "$REVIEW_JSON")\`"
  echo "Blocking review result is malformed or stale" >&2
  exit 1
fi

has_blocking_findings="$(jq -r '.has_blocking_findings' <<< "$REVIEW_JSON")"
max_severity="$(jq -r '.max_severity' <<< "$REVIEW_JSON")"
blocking_count="$(jq '[.findings[] | select(.severity == "HIGH" or .severity == "CRITICAL")] | length' <<< "$REVIEW_JSON")"

if [[ "$has_blocking_findings" == "false" && "$max_severity" == "NONE" && "$blocking_count" == "0" ]]; then
  write_summary \
    "## AI Blocking Review" \
    "" \
    "No critical or high issues were reported for \`$HEAD_SHA\`."
  exit 0
fi

write_summary \
  "## AI Blocking Review" \
  "" \
  "Critical or high issues were reported for \`$HEAD_SHA\`. The PR is blocked until the findings are fixed or the review is rerun with no blocking findings." \
  ""

jq -r '
  .findings[]
  | select(.severity == "HIGH" or .severity == "CRITICAL")
  | "- **\(.severity)** \(.path // "unknown")\(if (.line // null) then ":\(.line)" else "" end): \(.title)"
' <<< "$REVIEW_JSON" | while IFS= read -r finding; do
  write_summary "$finding"
done

echo "AI blocking review found critical or high issues" >&2
exit 1
