#!/usr/bin/env bash
# Runs the canonical historical-state provider benchmark and saves its Criterion
# results under the baseline supplied as the first argument.
set -euo pipefail

baseline="${1:?usage: run_storage_bench.sh BASELINE}"

cargo bench --locked -p base-node-core --bench provider -- \
  --save-baseline "$baseline" --noplot
