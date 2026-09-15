#!/usr/bin/env bash
# Runs the curated deterministic iai-callgrind benchmark subset, printing raw
# instruction counts to stdout for iai_compare.py to diff base against head.
#
# bench-iai.yml hoists the head commit's copy of this script and runs it against
# both the base and head trees, so an identical bench list runs on each side. A
# bench that does not exist on the base commit — e.g. the PR that first introduces
# it — simply produces no output for that side and is reported as "new" rather
# than failing the run. `set -e` is deliberately omitted so one missing or broken
# bench never suppresses the others; this job is advisory and never gates a merge.
set -uo pipefail

run() {
  echo "::group::$*"
  "$@"
  echo "::endgroup::"
}

run cargo bench -p base-common-flz --bench flz_iai
run cargo bench -p base-protocol --bench frame_parse_iai
run cargo bench -p base-protocol --bench batch_transaction_iai
run cargo bench -p base-consensus-derive --bench batch_queue_iai --features test-utils
run cargo bench -p base-common-flashblocks --bench flashblock_decode_iai
run cargo bench -p base-execution-txpool --bench validity_iai

exit 0
