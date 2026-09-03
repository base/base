#!/usr/bin/env bash
set -uo pipefail

# Machine-checkable acceptance gate for the validity stress workload.

PROMETHEUS_URL="${PROMETHEUS_URL:-http://localhost:9090}"
WINDOW="${WINDOW:-1m}"
PREDICATE_PROFILE="${PREDICATE_PROFILE:-cold}"
MIN_COMPLETED_BUILDS="${MIN_COMPLETED_BUILDS:-200}"
MIN_CANDIDATES_PER_BUILD="${MIN_CANDIDATES_PER_BUILD:-25}"
MIN_PREDICATE_SLOT_READS_PER_BUILD="${MIN_PREDICATE_SLOT_READS_PER_BUILD:-25}"
COLD_MIN_UNIQUE_PREDICATE_SLOT_FRACTION="${COLD_MIN_UNIQUE_PREDICATE_SLOT_FRACTION:-0.50}"
WARM_MAX_UNIQUE_PREDICATE_SLOT_FRACTION="${WARM_MAX_UNIQUE_PREDICATE_SLOT_FRACTION:-0.10}"
MIN_DEFERRED_PER_BUILD="${MIN_DEFERRED_PER_BUILD:-5}"
MAX_PRIMARY_COVERAGE="${MAX_PRIMARY_COVERAGE:-0.90}"
MIN_CUTOFF_BUILD_FRACTION="${MIN_CUTOFF_BUILD_FRACTION:-0.80}"
WAIT_CONSECUTIVE_SECONDS="${WAIT_CONSECUTIVE_SECONDS:-30}"
WAIT_TIMEOUT_SECONDS="${WAIT_TIMEOUT_SECONDS:-600}"
POLL_INTERVAL_SECONDS="${POLL_INTERVAL_SECONDS:-15}"
PROMETHEUS_TIMEOUT_SECONDS="${PROMETHEUS_TIMEOUT_SECONDS:-5}"
JOB="${BUILDER_JOB:-l2_builder}"

usage() {
  echo "Usage: $0 [--wait]" >&2
  echo "Set PREDICATE_PROFILE=cold|warm (default: cold); thresholds and connection settings use environment variables." >&2
}

WAIT=false
case "${1:-}" in
  "") ;;
  --wait) WAIT=true ;;
  -h|--help) usage; exit 0 ;;
  *) usage; exit 2 ;;
esac
if (( $# > 1 )); then usage; exit 2; fi

for command in curl jq; do
  command -v "$command" >/dev/null 2>&1 || { echo "missing required command: $command" >&2; exit 2; }
done
[[ "$WINDOW" =~ ^[1-9][0-9]*(ms|s|m|h|d|w|y)$ ]] || { echo "invalid WINDOW: $WINDOW" >&2; exit 2; }
[[ "$PREDICATE_PROFILE" == cold || "$PREDICATE_PROFILE" == warm ]] || { echo "invalid PREDICATE_PROFILE: $PREDICATE_PROFILE (expected cold or warm)" >&2; exit 2; }
for setting in MIN_COMPLETED_BUILDS MIN_CANDIDATES_PER_BUILD MIN_PREDICATE_SLOT_READS_PER_BUILD COLD_MIN_UNIQUE_PREDICATE_SLOT_FRACTION WARM_MAX_UNIQUE_PREDICATE_SLOT_FRACTION MIN_DEFERRED_PER_BUILD MAX_PRIMARY_COVERAGE MIN_CUTOFF_BUILD_FRACTION WAIT_CONSECUTIVE_SECONDS WAIT_TIMEOUT_SECONDS POLL_INTERVAL_SECONDS PROMETHEUS_TIMEOUT_SECONDS; do
  value="${!setting}"
  [[ "$value" =~ ^([0-9]+([.][0-9]*)?|[.][0-9]+)$ ]] || { echo "invalid $setting: $value" >&2; exit 2; }
done

prometheus_value() {
  local query="$1" response value
  response="$(curl -fsS --max-time "$PROMETHEUS_TIMEOUT_SECONDS" -G \
    --data-urlencode "query=$query" "$PROMETHEUS_URL/api/v1/query" 2>/dev/null)" || return 1
  value="$(jq -er '
    select(.status == "success")
    | select(.data.result | length == 1)
    | .data.result[0].value[1]
    | select(test("^(NaN|[+-]Inf)$") | not)
    | tonumber
  ' <<<"$response" 2>/dev/null)" || return 1
  printf '%s' "$value"
}

number_is() {
  local value="$1" operator="$2" threshold="$3"
  jq -ne --argjson value "$value" --argjson threshold "$threshold" \
    "\$value $operator \$threshold" >/dev/null
}

run_gate() {
  local selector="job=\"$JOB\"" builds evaluated deferred slots unique_slots cutoff wakeups rescans included errors up
  local candidates_per_build="" slots_per_build="" unique_slot_fraction="" deferred_per_build="" coverage="" cutoff_fraction=""
  local query_failed=false failed=0

  up="$(prometheus_value "max(up{$selector})")" || { up="N/A"; query_failed=true; }
  builds="$(prometheus_value "sum(increase(reth_base_builder_validity_predicate_candidates_evaluated_per_build_count{$selector}[$WINDOW]))")" || { builds="N/A"; query_failed=true; }
  evaluated="$(prometheus_value "sum(increase(reth_base_builder_validity_predicate_candidates_evaluated_per_build_sum{$selector}[$WINDOW])) or vector(0)")" || { evaluated="N/A"; query_failed=true; }
  deferred="$(prometheus_value "sum(increase(reth_base_builder_validity_predicate_candidates_deferred_per_build_sum{$selector}[$WINDOW])) or vector(0)")" || { deferred="N/A"; query_failed=true; }
  slots="$(prometheus_value "sum(increase(reth_base_builder_predicate_slots_loaded_total_sum{$selector}[$WINDOW])) or vector(0)")" || { slots="N/A"; query_failed=true; }
  unique_slots="$(prometheus_value "sum(increase(reth_base_builder_predicate_slots_loaded_unique_sum{$selector}[$WINDOW])) or vector(0)")" || { unique_slots="N/A"; query_failed=true; }
  cutoff="$(prometheus_value "sum(increase(reth_base_builder_validity_predicate_eval_cutoff_builds_total{$selector}[$WINDOW])) or vector(0)")" || { cutoff="N/A"; query_failed=true; }
  wakeups="$(prometheus_value "sum(increase(reth_base_builder_predicate_bucket_wakeups_sum{$selector}[$WINDOW])) or vector(0)")" || { wakeups="N/A"; query_failed=true; }
  rescans="$(prometheus_value "sum(increase(reth_base_builder_validity_predicate_evaluations_total{$selector,outcome=~\"rescan_matched|rescan_not_satisfied|rescan_budget_exhausted\"}[$WINDOW])) or vector(0)")" || { rescans="N/A"; query_failed=true; }
  included="$(prometheus_value "sum(increase(reth_base_builder_txs_included_per_block_sum{$selector,flow=\"validity\"}[$WINDOW])) or vector(0)")" || { included="N/A"; query_failed=true; }
  errors="$(prometheus_value "sum(increase(reth_base_builder_validity_predicate_evaluations_total{$selector,outcome=~\"read_error|rescan_read_error\"}[$WINDOW])) or vector(0)")" || { errors="N/A"; query_failed=true; }

  if [[ "$query_failed" == false ]] && number_is "$builds" '>' 0 && number_is "$(jq -n "$evaluated + $deferred")" '>' 0; then
    candidates_per_build="$(jq -nr "($evaluated + $deferred) / $builds")"
    slots_per_build="$(jq -nr "$slots / $builds")"
    if number_is "$slots" '>' 0; then
      unique_slot_fraction="$(jq -nr "$unique_slots / $slots")"
    fi
    deferred_per_build="$(jq -nr "$deferred / $builds")"
    coverage="$(jq -nr "$evaluated / ($evaluated + $deferred)")"
    cutoff_fraction="$(jq -nr "$cutoff / $builds")"
  fi

  printf '\nValidity stress gate (profile=%s, window=%s, job=%s)\n' "$PREDICATE_PROFILE" "$WINDOW" "$JOB"
  printf '%-30s %12s %12s %s\n' "goal" "value" "threshold" "result"
  check() {
    local name="$1" value="$2" operator="$3" threshold="$4" status="FAIL"
    if [[ -n "$value" && "$value" != N/A ]] && number_is "$value" "$operator" "$threshold"; then status="PASS"; else failed=1; fi
    printf '%-30s %12s %12s %s\n' "$name" "${value:-N/A}" "$operator $threshold" "$status"
  }
  check "builder up" "$up" '==' 1
  check "completed builds" "$builds" '>=' "$MIN_COMPLETED_BUILDS"
  check "validity candidates/build" "$candidates_per_build" '>=' "$MIN_CANDIDATES_PER_BUILD"
  check "predicate slot reads/build" "$slots_per_build" '>=' "$MIN_PREDICATE_SLOT_READS_PER_BUILD"
  if [[ "$PREDICATE_PROFILE" == cold ]]; then
    check "unique predicate slot fraction" "$unique_slot_fraction" '>=' "$COLD_MIN_UNIQUE_PREDICATE_SLOT_FRACTION"
  else
    check "unique predicate slot fraction" "$unique_slot_fraction" '<=' "$WARM_MAX_UNIQUE_PREDICATE_SLOT_FRACTION"
  fi
  check "deferred/build" "$deferred_per_build" '>=' "$MIN_DEFERRED_PER_BUILD"
  check "primary coverage" "$coverage" '<=' "$MAX_PRIMARY_COVERAGE"
  check "cutoff-build fraction" "$cutoff_fraction" '>=' "$MIN_CUTOFF_BUILD_FRACTION"
  check "bucket wakeups" "$wakeups" '>' 0
  check "rescan outcomes" "$rescans" '>' 0
  check "validity inclusions" "$included" '>' 0
  check "predicate read errors" "$errors" '==' 0
  return "$failed"
}

if [[ "$WAIT" == false ]]; then
  run_gate
  exit $?
fi

started="$(date +%s)"
passing_since=""
while true; do
  now="$(date +%s)"
  if run_gate; then
    [[ -n "$passing_since" ]] || passing_since="$now"
    if (( now - passing_since >= WAIT_CONSECUTIVE_SECONDS )); then
      echo "PASS: rolling-window goals passed consecutive polls for at least ${WAIT_CONSECUTIVE_SECONDS}s"
      exit 0
    fi
    echo "passing for $((now - passing_since))/${WAIT_CONSECUTIVE_SECONDS}s"
  else
    passing_since=""
  fi
  if (( now - started >= WAIT_TIMEOUT_SECONDS )); then
    echo "FAIL: timed out after ${WAIT_TIMEOUT_SECONDS}s" >&2
    exit 1
  fi
  sleep "$POLL_INTERVAL_SECONDS"
done
