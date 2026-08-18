#!/usr/bin/env bash
# Runs the curated advisory benchmark subset in the current working directory,
# saving each result under the `current` criterion baseline for base-vs-head
# comparison.
#
# bench-pr.yml invokes this once per side (base and head) from the *head* checkout
# so a single authoritative list runs against both trees; the only difference is the
# working directory the caller sets. Keeping the list here removes the drift risk of
# duplicating it across two workflow steps, where adding or removing a bench in only
# one block would surface as a spurious "new"/"missing" coverage change.
#
# The first argument selects a phase:
#   compile  build the bench binaries only (cargo bench --no-run). Compilation does
#            not affect measurement, so the caller runs this for base and head
#            concurrently to keep the (CPU-bound) build off the critical path.
#   measure  run the already-built benches and save the `current` baseline. The
#            caller runs this serially on one host so a reported delta reflects the
#            code change rather than host-to-host variance or build/run contention.
# Defaults to measure so a bare invocation keeps its original behavior.
#
# Advisory only: a bench that fails to compile or run must not fail the job, so the
# caller runs this with continue-on-error and the script omits `set -e` so one bad
# bench does not abort the rest. The curated subset is the deterministic, CPU-bound
# benches; I/O-heavy, async, and multi-threaded benches are left out because their
# wall-clock time is too noisy to read per-PR.
set -uo pipefail

MODE="${1:-measure}"

run() { echo "::group::$*"; "$@"; echo "::endgroup::"; }

# Each entry is a cargo target selector (package, bench, features) followed by `--`
# and the criterion runtime args. In compile mode only the selector runs, with
# --no-run; in measure mode the full command runs and saves the `current` baseline.
bench() {
  local selector=() run_args=() seen_sep=false
  for arg in "$@"; do
    if [ "$arg" = "--" ]; then
      seen_sep=true
    elif $seen_sep; then
      run_args+=("$arg")
    else
      selector+=("$arg")
    fi
  done

  if [ "$MODE" = "compile" ]; then
    run cargo bench --no-run "${selector[@]}"
  else
    run cargo bench "${selector[@]}" -- "${run_args[@]}"
  fi
}

bench -p base-proof-mpt --bench trie_node -- --save-baseline current --noplot
bench -p base-protocol --bench batch_transaction -- --save-baseline current --noplot
bench -p base-consensus-derive --bench batch_queue --features test-utils \
  -- --save-baseline current --noplot
bench -p base-common-precompiles --bench base_precompiles --features test-utils \
  -- --save-baseline current --noplot
bench -p base-builder-core --bench tx_selection -- --save-baseline current --noplot
bench -p base-flashblocks-node --bench sender_recovery \
  -- --save-baseline current sequential --noplot
bench -p base-common-flz --bench flz -- --save-baseline current --noplot
bench -p base-common-flashblocks --bench flashblock_decode -- --save-baseline current --noplot
bench -p base-protocol --bench frame_parse -- --save-baseline current --noplot
