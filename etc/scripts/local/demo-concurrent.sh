#!/usr/bin/env bash
# Sweeps the concurrent demo of the native resting-sweep bench
# (crates/execution/payload/benches/native_resting_sweep.rs): for every submitter count
# and target ingest rate, one process admits the parked validity population, then builds
# blocks on a 200 ms cadence while submitter threads keep admitting parked transactions.
# Each run's full output goes to its own log; the CONCURRENT summary lines are printed
# and collected in summary.txt.
#
# Usage: demo-concurrent.sh
#
# Environment:
#   SUBMITTERS   space-separated submitter counts (default: "1 2 4 8")
#   RATES        space-separated aggregate target rates in tx/s, or "free" to admit as
#                fast as the pool accepts (default: "free")
#   BENCH_BIN    a prebuilt native_resting_sweep binary (default: build it with cargo)
#   OUT_DIR      where logs go (default: target/demo-concurrent/<UTC timestamp>)
#   RESTING_MATRIX_POPULATIONS, RESTING_MATRIX_BLOCKS, RESTING_MATRIX_MODES
#                passed through (defaults: 1000000, 15, enforce); every other
#                RESTING_MATRIX_* variable, such as RESTING_MATRIX_DEMO_BATCH, passes
#                through unchanged.
#
# Example: SUBMITTERS=4 RATES="10000 50000 100000" etc/scripts/local/demo-concurrent.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo_root"

submitters="${SUBMITTERS:-1 2 4 8}"
rates="${RATES:-free}"
out_dir="${OUT_DIR:-target/demo-concurrent/$(date -u +%Y%m%dT%H%M%SZ)}"
export RESTING_MATRIX_CASES=demo
export RESTING_MATRIX_POPULATIONS="${RESTING_MATRIX_POPULATIONS:-1000000}"
export RESTING_MATRIX_BLOCKS="${RESTING_MATRIX_BLOCKS:-15}"
export RESTING_MATRIX_MODES="${RESTING_MATRIX_MODES:-enforce}"

bench_bin="${BENCH_BIN:-}"
if [[ -z "$bench_bin" ]]; then
  bench_bin="$(cargo bench -p base-execution-payload-builder --bench native_resting_sweep --no-run 2>&1 \
    | tee /dev/stderr | sed -n 's/.*Executable .*(\(.*\))$/\1/p' | tail -n 1)"
  [[ -n "$bench_bin" ]] || { echo "could not locate the native_resting_sweep binary" >&2; exit 1; }
fi

mkdir -p "$out_dir"
echo "binary=$bench_bin out_dir=$out_dir population=$RESTING_MATRIX_POPULATIONS blocks=$RESTING_MATRIX_BLOCKS modes=$RESTING_MATRIX_MODES"
for count in $submitters; do
  for rate in $rates; do
    log="$out_dir/submitters-${count}-rate-${rate}.log"
    rate_env=()
    [[ "$rate" == free ]] || rate_env=(RESTING_MATRIX_DEMO_RATE="$rate")
    if env RESTING_MATRIX_DEMO_SUBMITTERS="$count" ${rate_env[@]+"${rate_env[@]}"} "$bench_bin" >"$log" 2>&1; then
      grep '^CONCURRENT ' "$log" | tee -a "$out_dir/summary.txt"
    else
      echo "FAILED submitters=$count rate=$rate; see $log" | tee -a "$out_dir/summary.txt"
    fi
  done
done
