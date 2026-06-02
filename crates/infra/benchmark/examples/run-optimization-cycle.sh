#!/usr/bin/env bash
set -euo pipefail

BASELINE_GROUP_ID=""
NODE_ARGS=""
OUTPUT_DIR="./results"
RESULTS_FILE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --baseline-group-id) BASELINE_GROUP_ID="$2"; shift 2 ;;
        --node-args)         NODE_ARGS="$2"; shift 2 ;;
        --output-dir)        OUTPUT_DIR="$2"; shift 2 ;;
        --results)           RESULTS_FILE="$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--baseline-group-id <id>] [--node-args <args>] [--output-dir <dir>] [--results <path>]" >&2
            exit 1
            ;;
    esac
done

RESULTS_FILE="${RESULTS_FILE:-${OUTPUT_DIR}/results.jsonl}"

get_last_run_group_id() {
    tail -1 "$1" | python3 -c "import sys,json; print(json.load(sys.stdin)['run_group_id'])"
}

if [[ -z "$BASELINE_GROUP_ID" ]]; then
    echo "==> Running baseline (no extra node args)..."
    base-bench --output-dir "$OUTPUT_DIR"
    BASELINE_GROUP_ID=$(get_last_run_group_id "$RESULTS_FILE")
    echo "==> Baseline run_group_id: $BASELINE_GROUP_ID"
fi

echo "==> Running challenger..."
if [[ -n "$NODE_ARGS" ]]; then
    base-bench --output-dir "$OUTPUT_DIR" --tags "role=challenger"
else
    base-bench --output-dir "$OUTPUT_DIR" --tags "role=challenger"
fi
CHALLENGER_GROUP_ID=$(get_last_run_group_id "$RESULTS_FILE")
echo "==> Challenger run_group_id: $CHALLENGER_GROUP_ID"

echo "==> Comparing baseline=$BASELINE_GROUP_ID vs challenger=$CHALLENGER_GROUP_ID ..."
base-bench compare \
    --results "$RESULTS_FILE" \
    --baseline "$BASELINE_GROUP_ID" \
    --challenger "$CHALLENGER_GROUP_ID"
