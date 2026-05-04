# base-benchmark

End-to-end throughput and latency benchmark tool for Base sequencer and validator nodes.

## Architecture

```text
base-bench
  ├── PortManager        – allocates ephemeral ports for each subprocess
  ├── SnapshotManager    – prepares (or caches) chain snapshots before each run
  ├── BaseRethNodeClient / BuilderClient  – launches sequencer/builder subprocess
  ├── RPC Proxy (axum)   – intercepts eth_sendRawTransaction → FakeMempool
  ├── LoadTestPayloadWorker  – spawns base-load-test with temp YAML config
  ├── SequencerConsensusClient  – drives FCU→getPayload→newPayload loop
  ├── MetricsCollector   – scrapes Prometheus metrics after each block
  └── NetworkBenchmark   – top-level orchestrator; writes per-run JSON results
```

## Usage

```bash
BASE_BENCH_PREFUND_KEY=0x<key> base-bench \
  --config examples/devnet.yaml \
  --output-dir /tmp/bench-results \
  [--reth-bin /path/to/op-reth] \
  [--builder-bin /path/to/op-builder] \
  [--load-test-bin /path/to/base-load-test]
```

Binary paths default to siblings of the `base-bench` executable. All flags also
accept `BASE_BENCH_*` environment variables.

## Config format

See [`examples/devnet.yaml`](examples/devnet.yaml) for an annotated example.

| field | description |
|---|---|
| `block_time_ms` | Target block time in milliseconds |
| `num_blocks` | Number of blocks to produce per run |
| `flashblocks` | Optional flashblocks config (`block_time_ms`) |
| `transaction_payloads` | List of payload definitions (id, sender_count, transactions) |
| `benchmarks` | List of node definitions (node_type, snapshot, metrics thresholds) |

### Matrix expansion

Each benchmark entry is expanded against all payload entries. If a benchmark
declares `variables`, those are expanded combinatorially. The total number of
runs must not exceed 100.

## Output

Each run writes a JSON file to `--output-dir/<run-id>.json` containing per-block
metrics (gas, transactions, latencies) and threshold violations. The binary exits
non-zero if any run breaches an `error`-severity threshold.
