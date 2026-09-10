#!/usr/bin/env bash
# Runs the curated advisory benchmark subset in the current working directory,
# saving each result under the criterion baseline named by the first argument
# (default: `current`) for base-vs-head comparison.
#
# Usage: run_bench_subset.sh [BASELINE]
#
# bench-pr.yml invokes this twice from a single checkout — once per commit — after
# git-switching the working tree between the PR base and head. Both passes share one
# target dir, so the second build is incremental over the first; passing distinct
# baseline names ("pr-base" and "pr-head") keeps both results side by side under
# target/criterion for bench_compare.py to read. The caller runs the *hoisted* copy
# of this script (outside the tree) for both passes so a single authoritative list
# runs against both commits: keeping the list here removes the drift risk of
# duplicating it across two workflow steps, where adding or removing a bench in only
# one block would surface as a spurious "new"/"missing" coverage change.
#
# Advisory only: a bench that fails to compile or run must not fail the job, so the
# caller runs this with continue-on-error. The curated subset is the deterministic,
# CPU-bound benches; I/O-heavy, async, and multi-threaded benches are left out
# because their wall-clock time is too noisy to read per-PR.
set -uo pipefail

baseline="${1:-current}"

run() { echo "::group::$*"; "$@"; echo "::endgroup::"; }

run cargo bench -p base-proof-witness-mpt --bench trie_node \
  -- --save-baseline "$baseline" --noplot
run cargo bench -p base-consensus-batch --bench batch_transaction \
  -- --save-baseline "$baseline" --noplot
run cargo bench -p base-consensus-derive --bench batch_queue --features test-utils \
  -- --save-baseline "$baseline" --noplot
run cargo bench -p base-execution-evm-runtime --bench core_precompiles_base_precompiles --features test-utils \
  -- --save-baseline "$baseline" --noplot
run cargo bench -p base-common-chain-config --bench flz \
  -- --save-baseline "$baseline" --noplot
run cargo bench -p base-consensus-batch --bench frame_parse \
  -- --save-baseline "$baseline" --noplot
