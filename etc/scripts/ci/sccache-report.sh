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
misses="$(stat_field "Cache misses")"
misses="${misses:-0}"
hit_rate="$(stat_field "Cache hits rate")"
rust_hits="$(stat_field "Cache hits (Rust)")"
rust_misses="$(stat_field "Cache misses (Rust)")"
cxx_hits="$(stat_field "Cache hits (C/C++)")"
cxx_misses="$(stat_field "Cache misses (C/C++)")"
writes_stuck=0
if [[ "$misses" =~ ^[0-9]+$ && "$write_errors" =~ ^[0-9]+$ ]]; then
  writes_stuck=$((misses - write_errors))
  if (( writes_stuck < 0 )); then
    writes_stuck=0
  fi
fi

rate_limited=0
cache_full=0
auth=0
timeout=0
readonly_skip=0
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
  elif [[ "$lower" == *read.only* || "$lower" == *readonly* || "$lower" == *"not writable"* || "$lower" == *"skipping write"* ]]; then
    readonly_skip=$((readonly_skip + 1))
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
  echo "| Cache misses | \`$misses\` |"
  echo "| Cache write errors (stats) | \`$write_errors\` |"
  echo "| Writes that stuck | \`$writes_stuck\` |"
  echo "| Error-log lines | \`$classified_lines\` |"
  echo "| Rate limited | \`$rate_limited\` |"
  echo "| Cache full / quota | \`$cache_full\` |"
  echo "| Auth | \`$auth\` |"
  echo "| Timeout | \`$timeout\` |"
  echo "| Read-only skip | \`$readonly_skip\` |"
  echo "| Other log lines | \`$other\` |"
  echo
  if [[ "$mode" == "READ_ONLY" && "$write_errors" == "$misses" && "$misses" != "0" ]]; then
    echo "Read-only job: write errors equal misses. That is consistent with sccache counting skipped writes rather than GHA upload failures. Confirm from the log sample below (look for PUT/429 vs read-only skip)."
    echo
  fi
  if [[ "$write_errors" != "0" && "$classified_lines" -eq 0 ]]; then
    echo "Stats reported write errors but \`SCCACHE_ERROR_LOG\` was empty. Typical for the GHA 200 uploads/min rate limit, and for read-only jobs that count skipped writes as errors."
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
    echo "<details><summary>SCCACHE_ERROR_LOG (last 120 lines)</summary>"
    echo
    echo '```'
    tail -n 120 "$error_log"
    echo '```'
    echo
    echo "</details>"
  fi
} >> "$summary"

echo "sccache mode=$mode misses=$misses write_errors=$write_errors writes_stuck=$writes_stuck rate_limited=$rate_limited cache_full=$cache_full auth=$auth timeout=$timeout readonly_skip=$readonly_skip other=$other"
