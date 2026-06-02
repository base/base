# AI Agent Protocol for base-bench

## Overview

`base-bench` is a self-contained benchmark orchestrator for Base EL clients. An AI agent can use it to autonomously test performance hypotheses by running benchmarks, comparing results, and iterating.

## Quick Start

```bash
# Run the default Uniswap V3 benchmark (20 blocks, no args required)
base-bench

# Results written to ./results/
# Per-run index: ./results/results.jsonl
```

## Output Schema

Each benchmark run appends one JSON line to `results/results.jsonl`:

| Field | Type | Description |
|-------|------|-------------|
| `run_id` | string | Unique run identifier |
| `run_group_id` | string | Group identifier for this invocation |
| `git_sha` | string | Git commit SHA |
| `git_branch` | string | Git branch name |
| `tags` | object | Key-value metadata tags |
| `gas_per_second_sequencer` | float | Mean sequencer gas/s (throughput) |
| `get_payload_ms` | float | Mean `getPayload` latency (seconds, despite field name) |
| `new_payload_ms` | float | Mean `newPayload` latency (seconds, despite field name) |
| `success` | bool | Whether the run passed all thresholds |

## Comparing Runs

```bash
base-bench compare \
    --results ./results/results.jsonl \
    --baseline <baseline-run-group-id> \
    --challenger <challenger-run-group-id>
```

Exit codes:
- `0` — challenger is **better** (≥2% improvement in throughput or latency)
- `1` — challenger is **worse** (≥2% regression)
- `2` — **neutral** or error (insufficient difference)

## Running a Hypothesis

Use the provided harness script:

```bash
examples/run-optimization-cycle.sh \
    --node-args "--txpool.pending-max-count 65536" \
    --output-dir ./results
```

Or see `examples/hypothesis.yaml` for a structured template to document hypotheses.

## Key Metrics

| Metric | Direction | Trading Impact |
|--------|-----------|----------------|
| `gas_per_second_sequencer` | maximize | Higher throughput = more transactions per block |
| `get_payload_ms` | minimize | Lower latency = faster block building = faster trade inclusion |
| `new_payload_ms` | minimize | Lower latency = faster validator replay = faster finality |

## Tagging

```bash
base-bench --tags "hypothesis=my-change,variant=v1"
```

Tags appear in `results.jsonl` and can be used to filter/group runs.

## Agent Workflow

```
1. Run baseline:    base-bench --tags "hypothesis=baseline"
2. Record:          Extract run_group_id from last line of results.jsonl
3. Apply change:    Modify node args, config, or source code (recompile if needed)
4. Run challenger:  base-bench --tags "hypothesis=my-change"
5. Compare:         base-bench compare --baseline <id> --challenger <id>
6. Interpret:       Exit code 0 = improvement, 1 = regression, 2 = neutral
7. Iterate:         Refine hypothesis based on results
```
