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
# Advisory only: a bench that fails to compile or run must not fail the job, so the
# caller runs this with continue-on-error. The curated subset is the deterministic,
# CPU-bound benches; I/O-heavy, async, and multi-threaded benches are left out
# because their wall-clock time is too noisy to read per-PR.
set -uo pipefail

run() { echo "::group::$*"; "$@"; echo "::endgroup::"; }

run cargo bench -p base-proof-mpt --bench trie_node \
  -- --save-baseline current --noplot
run cargo bench -p base-protocol --bench batch_transaction \
  -- --save-baseline current --noplot
run cargo bench -p base-consensus-derive --bench batch_queue --features test-utils \
  -- --save-baseline current --noplot
run cargo bench -p base-common-precompiles --bench base_precompiles --features test-utils \
  -- --save-baseline current --noplot
run cargo bench -p base-builder-core --bench tx_selection \
  -- --save-baseline current --noplot
run cargo bench -p base-flashblocks-node --bench sender_recovery \
  -- --save-baseline current sequential --noplot
run cargo bench -p base-common-flz --bench flz \
  -- --save-baseline current --noplot
run cargo bench -p base-common-flashblocks --bench flashblock_decode \
  -- --save-baseline current --noplot
run cargo bench -p base-protocol --bench frame_parse \
  -- --save-baseline current --noplot
