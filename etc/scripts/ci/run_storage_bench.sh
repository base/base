#!/usr/bin/env bash
set -euo pipefail

baseline="${1:?usage: run_storage_bench.sh BASELINE}"

cargo bench --locked -p base-node-core --bench provider -- \
  --save-baseline "$baseline" --noplot
