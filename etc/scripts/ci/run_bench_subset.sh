#!/usr/bin/env bash
# Builds or runs the curated advisory benchmark subset.
#
# Usage:
#   run_bench_subset.sh build OUTPUT_DIR
#   run_bench_subset.sh run BASE_BIN_DIR HEAD_BIN_DIR
# Passing the same directory for both run arguments performs a full A/A diagnostic.
#
# bench-pr.yml hoists the head commit's copy of this script, then uses `build`
# once per commit. Each compiled benchmark executable is copied outside Cargo's
# shared target directory before the other commit is built. This cleanly separates
# compilation from measurement without giving up incremental builds between base
# and head.
#
# `run` measures each prebuilt executable in base-head-head-base (ABBA) order.
# Both revisions therefore occupy the same average position in time, reducing
# systematic bias from CPU frequency, temperature, and host-load drift. The two
# repetitions per revision also provide an A/A repeat-spread measurement for
# bench_compare.py. The long tx-selection executable is split by benchmark group
# so matching base and head samples remain close together in time. Each repetition
# uses half the old sample count and shorter warmup/measurement windows; across two
# repetitions each revision retains 100 samples while fitting comfortably in CI.
#
# Advisory only: a bench that fails to compile or run must not prevent the other
# benches from producing results. The curated subset contains deterministic,
# CPU-bound, single-threaded benches; I/O-heavy, async, and multi-threaded benches
# are excluded because their wall-clock time is too noisy to read per PR.
set -uo pipefail

mode="${1:-}"
failures=0

end_group() {
  local status="$1"
  echo "::endgroup::"
  if [ "$status" -ne 0 ]; then
    failures=1
  fi
}

find_bench_executable() {
  local bench="$1"
  python3 -c '
import json
import sys

bench = sys.argv[1]
executable = None
for line in sys.stdin:
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    if message.get("reason") == "compiler-message":
        rendered = message.get("message", {}).get("rendered")
        if rendered:
            print(rendered, file=sys.stderr, end="")
    target = message.get("target", {})
    if (
        message.get("reason") == "compiler-artifact"
        and target.get("name") == bench
        and "bench" in target.get("kind", [])
        and message.get("executable")
    ):
        executable = message["executable"]
if executable is None:
    raise SystemExit(f"cargo did not report an executable for benchmark {bench}")
print(executable)
' "$bench"
}

build_benchmark() {
  local key="$1"
  local package="$2"
  local bench="$3"
  shift 3

  echo "::group::Build $package --bench $bench"
  local executable
  if ! executable=$(cargo bench -p "$package" --bench "$bench" "$@" \
    --no-run --message-format=json-render-diagnostics | find_bench_executable "$bench"); then
    end_group 1
    return
  fi
  if ! cp "$executable" "$output_dir/$key"; then
    end_group 1
    return
  fi
  chmod +x "$output_dir/$key"
  end_group 0
}

run_once() {
  local side="$1"
  local key="$2"
  local baseline="$3"
  local filter="$4"
  local bin_dir="$base_bin_dir"
  if [ "$side" = "head" ]; then
    bin_dir="$head_bin_dir"
  fi

  echo "::group::Run $key ($side, $baseline)"
  if [ ! -x "$bin_dir/$key" ]; then
    echo "::warning::Missing $side benchmark executable: $key"
    end_group 1
    return
  fi

  local -a args=(
    # `cargo bench` normally supplies this hidden libtest-compatible flag. Direct
    # execution without it only smoke-tests each routine and saves no measurements.
    --bench
    --sample-size 50
    --warm-up-time 2
    --measurement-time 2
    --save-baseline "$baseline"
    --noplot
  )
  if [ -n "$filter" ]; then
    args=("$filter" "${args[@]}")
  fi
  "${bench_prefix[@]}" "$bin_dir/$key" "${args[@]}"
  end_group "$?"
}

run_pair() {
  local key="$1"
  local filter="${2:-}"
  run_once base "$key" pr-base-1 "$filter"
  run_once head "$key" pr-head-1 "$filter"
  run_once head "$key" pr-head-2 "$filter"
  run_once base "$key" pr-base-2 "$filter"
}

case "$mode" in
  build)
    output_dir="${2:?build requires OUTPUT_DIR}"
    mkdir -p "$output_dir"

    if [ -f crates/common/consensus/benches/sender_recovery.rs ]; then
      sender_package=base-common-consensus
      sender_features=(--features k256,serde)
    else
      # The base revision of the rollout PR still owns this benchmark in the node
      # crate, whose dev dependency on base-test-utils requires generated contracts.
      sender_package=base-flashblocks-node
      sender_features=()

      echo "::group::Build Solidity test contracts"
      (
        cd crates/utilities/test-utils/contracts || exit
        forge soldeer install && forge build
      )
      end_group "$?"
    fi

    build_benchmark trie_node base-proof-mpt trie_node
    build_benchmark batch_transaction base-protocol batch_transaction
    build_benchmark batch_queue base-consensus-derive batch_queue --features test-utils
    build_benchmark base_precompiles base-common-precompiles base_precompiles \
      --features test-utils
    build_benchmark tx_selection base-builder-core tx_selection
    build_benchmark sender_recovery "$sender_package" sender_recovery "${sender_features[@]}"
    build_benchmark flz base-common-flz flz
    build_benchmark flashblock_decode base-common-flashblocks flashblock_decode
    build_benchmark frame_parse base-protocol frame_parse
    ;;
  run)
    base_bin_dir="${2:?run requires BASE_BIN_DIR}"
    head_bin_dir="${3:?run requires HEAD_BIN_DIR}"
    export CRITERION_HOME="${CRITERION_HOME:-$PWD/target/criterion}"
    # Cached target directories can contain identically named baselines from an
    # earlier workflow run. Never let those mask a failed or missing repetition.
    rm -rf "$CRITERION_HOME"

    allowed_cpus=$(awk '/Cpus_allowed_list/ { print $2 }' /proc/self/status)
    bench_cpu="${BENCH_CPU:-${allowed_cpus%%[-,]*}}"
    bench_prefix=()
    if command -v taskset > /dev/null; then
      bench_prefix=(taskset -c "$bench_cpu")
      echo "Benchmark CPU: $bench_cpu (allowed: $allowed_cpus)"
    else
      echo "::warning::taskset unavailable; running benchmarks without CPU affinity"
    fi

    run_pair trie_node
    run_pair batch_transaction
    run_pair batch_queue
    run_pair base_precompiles
    run_pair tx_selection 'tx_selection/best_transactions/'
    run_pair tx_selection 'tx_selection/best_transactions_chained/'
    run_pair tx_selection 'tx_selection/parkable_payload/'
    run_pair tx_selection 'tx_selection/predicate_rescan/'
    run_pair tx_selection 'tx_selection/predicate_index/'
    run_pair sender_recovery sequential
    run_pair flz
    run_pair flashblock_decode
    run_pair frame_parse
    ;;
  *)
    echo "usage: $0 build OUTPUT_DIR | run BASE_BIN_DIR HEAD_BIN_DIR" >&2
    exit 2
    ;;
esac

exit "$failures"
