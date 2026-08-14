#!/usr/bin/env bash
set -euo pipefail

# Publishes sccache hit/miss/write-error stats to the GitHub job summary and
# classifies SCCACHE_ERROR_LOG lines so write failures are attributable
# (GHA 200 uploads/min rate limit vs quota vs auth vs timeout).

if ! command -v sccache >/dev/null 2>&1; then
  echo "sccache not installed; skipping report"
  exit 0
fi

stats="$(sccache --show-stats 2>&1 || true)"
error_log="${SCCACHE_ERROR_LOG:-${RUNNER_TEMP:-/tmp}/sccache-error.log}"
mode="${SCCACHE_GHA_RW_MODE:-unknown}"

stat_field() {
  local label="$1"
  printf '%s\n' "$stats" | awk -v label="$label" '
    index($0, label) == 1 {
      print $NF
      exit
    }
  '
}

write_errors="$(stat_field "Cache write errors")"
write_errors="${write_errors:-0}"
hit_rate="$(stat_field "Cache hits rate")"
rust_hits="$(stat_field "Cache hits (Rust)")"
rust_misses="$(stat_field "Cache misses (Rust)")"
cxx_hits="$(stat_field "Cache hits (C/C++)")"
cxx_misses="$(stat_field "Cache misses (C/C++)")"

rate_limited=0
cache_full=0
auth=0
timeout=0
other=0
classified_lines=0

classify_line() {
  local lower
  lower="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  classified_lines=$((classified_lines + 1))
  if [[ "$lower" == *429* || "$lower" == *ratelimit* || "$lower" == *"rate limit"* || "$lower" == *"too many requests"* ]]; then
    rate_limited=$((rate_limited + 1))
  elif [[ "$lower" == *quota* || "$lower" == *"no space"* || "$lower" == *"insufficient storage"* || "$lower" == *"cache too large"* || "$lower" == *"cache full"* ]]; then
    cache_full=$((cache_full + 1))
  elif [[ "$lower" == *401* || "$lower" == *403* || "$lower" == *unauthorized* || "$lower" == *forbidden* || "$lower" == *permission* || "$lower" == *unauthenticated* ]]; then
    auth=$((auth + 1))
  elif [[ "$lower" == *timeout* || "$lower" == *"timed out"* || "$lower" == *deadline* ]]; then
    timeout=$((timeout + 1))
  else
    other=$((other + 1))
  fi
}

if [[ -s "$error_log" ]]; then
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "${line// /}" ]] && continue
    classify_line "$line"
  done < "$error_log"
fi

summary="${GITHUB_STEP_SUMMARY:-/dev/stdout}"
{
  echo "### sccache"
  echo
  echo "| Field | Value |"
  echo "|---|---|"
  echo "| Mode | \`$mode\` |"
  echo "| Cache hits rate | ${hit_rate:-n/a} |"
  echo "| Rust hits / misses | ${rust_hits:-n/a} / ${rust_misses:-n/a} |"
  echo "| C/C++ hits / misses | ${cxx_hits:-n/a} / ${cxx_misses:-n/a} |"
  echo "| Cache write errors (stats) | \`$write_errors\` |"
  echo "| Error-log lines | \`$classified_lines\` |"
  echo "| Rate limited | \`$rate_limited\` |"
  echo "| Cache full / quota | \`$cache_full\` |"
  echo "| Auth | \`$auth\` |"
  echo "| Timeout | \`$timeout\` |"
  echo "| Other errors | \`$other\` |"
  echo
  if [[ "$mode" == "READ_ONLY" && "$write_errors" != "0" ]]; then
    echo "Write errors on a read-only job are unexpected; the GHA backend may have opened read-write before \`SCCACHE_GHA_RW_MODE\` was applied."
    echo
  fi
  if [[ "$write_errors" != "0" && "$classified_lines" -eq 0 ]]; then
    echo "Stats reported write errors but \`SCCACHE_ERROR_LOG\` was empty. The usual cause on this repo is the GitHub Actions cache upload rate limit (200/min/repo); sccache increments the counter without always logging a line."
    echo
  fi
  echo "<details><summary>sccache --show-stats</summary>"
  echo
  echo '```'
  printf '%s\n' "$stats"
  echo '```'
  echo
  echo "</details>"
  if [[ -s "$error_log" ]]; then
    echo
    echo "<details><summary>SCCACHE_ERROR_LOG (last 80 lines)</summary>"
    echo
    echo '```'
    tail -n 80 "$error_log"
    echo '```'
    echo
    echo "</details>"
  fi
} >> "$summary"

echo "sccache mode=$mode write_errors=$write_errors rate_limited=$rate_limited cache_full=$cache_full auth=$auth timeout=$timeout other=$other"
