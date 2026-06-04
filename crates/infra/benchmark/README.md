# base-benchmark

End-to-end throughput and latency benchmark for Base sequencer and validator nodes.

## Quick start

```bash
base-bench \
  --config examples/devnet.yaml \
  --reth-bin /path/to/base-reth-node \
  --load-test-bin /path/to/base-load-test
```

Results are written to `./results/`. See [`examples/devnet.yaml`](examples/devnet.yaml) for a working config.

## Flags

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `--config` | `BASE_BENCH_CONFIG` | embedded `devnet.yaml` | YAML config file |
| `--root-dir` | `BASE_BENCH_ROOT_DIR` | `.` | Working directory for results and snapshots |
| `--output-dir` | `BASE_BENCH_OUTPUT_DIR` | `<root-dir>/results` | Directory for run output |
| `--reth-bin` | `BASE_BENCH_RETH_BIN` | sibling of `base-bench` | Path to `base-reth-node` binary |
| `--builder-bin` | `BASE_BENCH_BUILDER_BIN` | sibling of `base-bench` | Path to `base-builder` binary |
| `--load-test-bin` | `BASE_BENCH_LOAD_TEST_BIN` | sibling of `base-bench` | Path to `base-load-test` binary |
| `--tags` | `BASE_BENCH_TAGS` | (none) | Comma-separated `key=value` pairs attached to results |

## Config format

```yaml
name: my-benchmark
block_time_ms: 2000
num_blocks: 20

benchmarks:
  - node_type: reth         # "reth" or "builder"
    datadir: {}             # {} = fresh devnet; or {sequencer: /path, validator: /path}
    payload:
      id: my-payload
      type: load-test
      params:
        sender_count: 50
        funding_amount: "1000000000000000000"  # 1 ETH per sender
        transactions:
          - weight: 1
            type: uniswap_v3
            fee: 3000
            min_amount: "1000000000000"
            max_amount: "10000000000000"
    metrics:
      warning:
        - metric: gas/per_block
          min: 1000000
      error:
        - metric: gas/per_second
          min: 100000000
```

### Transaction types

| `type` | Required fields | Notes |
|--------|----------------|-------|
| `transfer` | — | Simple ETH transfer |
| `calldata` | `max_size` | ETH transfer with random calldata |
| `erc20` | `contract` | ERC-20 transfer |
| `precompile` | `target`, `iterations` | Precompile call |
| `uniswap_v3` | `fee`, `min_amount`, `max_amount` | `router`/`token_in`/`token_out` auto-deployed if omitted |
| `aerodrome_cl` | `router`, `token_in`, `token_out`, `tick_spacing`, `min_amount`, `max_amount` | Aerodrome CL swap |

### Automatic Uniswap V3 setup

If a `uniswap_v3` entry has no `router` address, the runner deploys a `MockUniswapV3Router`
and two `FreeTransferERC20` tokens on the devnet before the load test starts, then injects
the addresses. Supply explicit addresses to use a real deployment (e.g. a mainnet snapshot).

### Matrix expansion

Add `variables` to a benchmark entry to run combinatorial sweeps:

```yaml
benchmarks:
  - node_type: reth
    datadir: {}
    payload: ...
    variables:
      - name: sender_count
        values: ["10", "50", "100"]
```

Each combination becomes a separate run. Maximum 100 runs per invocation.

## Output

Each run writes three files under `--output-dir/<run-id>/`:

| File | Contents |
|------|----------|
| `metadata.json` | Run config, git SHA/branch, tags, run group ID, per-metric averages |
| `metrics-sequencer.json` | Per-block metrics from the sequencer |
| `metrics-validator.json` | Per-block metrics from the validator |

A human-readable summary is also printed to stdout after each run.

