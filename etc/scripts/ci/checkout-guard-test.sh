#!/usr/bin/env bash
set -euo pipefail

# Exercise the actual inline workflow guards without running repository code.
WORKFLOW="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)/.github/workflows/feature-docs-draft.yml"
TMP_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMP_ROOT"' EXIT

extract_run_block() {
  local step_name="$1" occurrence="$2"
  awk -v step="$step_name" -v occurrence="$occurrence" '
    index($0, "- name: " step) > 0 { count++; found = count == occurrence; next }
    found && $0 ~ /^        run: \|/ { in_run = 1; next }
    in_run {
      if ($0 !~ /^          /) { exit }
      sub(/^          /, "")
      print
    }
  ' "$WORKFLOW"
}

run_guard_case() {
  local case_name="$1" expected_exit="$2" env_var="$3" value="$4" guard_script="$5"
  local repo actual_exit=0
  repo="$(mktemp -d "$TMP_ROOT/case.XXXXXX")"
  git -C "$repo" init --quiet
  git -C "$repo" config user.email "guard-test@example.invalid"
  git -C "$repo" config user.name "guard-test"
  # Fixture-only repository: no credentials or signing setup required.
  git -C "$repo" -c commit.gpgsign=false commit --quiet --allow-empty -m "guard-test fixture"
  if [ "$value" = "match" ]; then
    value="$(git -C "$repo" rev-parse HEAD)"
  fi
  (
    cd "$repo"
    env "$env_var=$value" SOURCE_PR=123 bash -c "$guard_script"
  ) >"$repo/guard.out" 2>&1 || actual_exit=$?
  if [ "$actual_exit" -ne "$expected_exit" ]; then
    echo "FAIL: $case_name: expected exit $expected_exit, got $actual_exit"
    cat "$repo/guard.out"
    exit 1
  fi
  echo "PASS: $case_name"
}

test_guard() {
  local step_name="$1" env_var="$2" occurrence="$3" guard_script
  guard_script="$(extract_run_block "$step_name" "$occurrence")"
  if [ -z "$guard_script" ]; then
    echo "FAIL: missing guard $step_name occurrence $occurrence"
    exit 1
  fi
  run_guard_case "$step_name [$occurrence]: match" 0 "$env_var" match "$guard_script"
  run_guard_case "$step_name [$occurrence]: drift" 1 "$env_var" 0000000000000000000000000000000000000000 "$guard_script"
  run_guard_case "$step_name [$occurrence]: invalid" 1 "$env_var" not-a-sha "$guard_script"
}

# Both control guards are checked independently so they cannot silently diverge.
test_guard "Verify checked-out HEAD matches the captured control SHA (inline, fail closed)" EXPECTED_CONTROL_SHA 1
test_guard "Verify checked-out HEAD matches the captured control SHA (inline, fail closed)" EXPECTED_CONTROL_SHA 2
test_guard "Verify checked-out HEAD matches the captured snapshot SHA (inline, fail closed)" EXPECTED_SNAPSHOT_SHA 1

echo "All nine checkout-guard cases passed"
