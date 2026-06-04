# base-benchmark

End-to-end throughput and latency benchmark for Base sequencer and validator nodes.

## Quick start

```bash
base-bench \
  --reth-bin /path/to/base-reth-node \
  --load-test-bin /path/to/base-load-test
```

No config required. Defaults to 20 blocks of Uniswap V3 swaps on a fresh devnet.
Results are written to `./results/`.

## Flags

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `--config` | `BASE_BENCH_CONFIG` | embedded `devnet.yaml` | YAML config file |
| `--root-dir` | `BASE_BENCH_ROOT_DIR` | `.` | Working directory for results and snapshots |
| `--output-dir` | `BASE_BENCH_OUTPUT_DIR` | `<root-dir>/results` | Directory for run output |
| `--reth-bin` | `BASE_BENCH_RETH_BIN` | sibling of `base-bench` | Path to `base-reth-node` binary |
| `--builder-bin` | `BASE_BENCH_BUILDER_BIN` | sibling of `base-bench` | Path to `base-builder` binary |
| `--load-test-bin` | `BASE_BENCH_LOAD_TEST_BIN` | sibling of `base-bench` | Path to `base-load-test` binary |
| `--prefund-key` | `BASE_BENCH_PREFUND_KEY` | Hardhat account #1 | Hex private key for pre-funding |
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
| `metadata.json` | Run config, git SHA/branch, tags, run group ID |
| `metrics-sequencer.json` | Per-block metrics from the sequencer |
| `metrics-validator.json` | Per-block metrics from the validator |

A summary line is also appended to `--output-dir/results.jsonl`:

```json
{
  "run_id": "abc123",
  "run_group_id": "xyz456",
  "git_sha": "abcdef",
  "git_branch": "my-branch",
  "config_name": "my-benchmark",
  "node_type": "reth",
  "success": true,
  "tags": {"hypothesis": "larger-txpool"},
  "gas_per_second_sequencer": 481138671,
  "get_payload_ms": 5.6,
  "new_payload_ms": 6.9
}
```

### Key metrics

| Metric | Description |
|--------|-------------|
| `gas_per_second_sequencer` | Average gas throughput over all benchmark blocks |
| `get_payload_ms` | Average `engine_getPayloadV4` latency (sequencer) |
| `new_payload_ms` | Average `engine_newPayloadV4` latency (validator) |

## Comparing runs

```bash
# Tag two runs differently, then compare by group ID
base-bench --tags hypothesis=baseline ...
base-bench --tags hypothesis=challenger ...

base-bench compare \
  --results ./results/results.jsonl \
  --baseline <baseline-run-group-id> \
  --challenger <challenger-run-group-id>
```

Exit code: `0` = challenger is better (>+2%), `1` = worse (>-2%), `2` = neutral.

The `run_group_id` for each invocation is printed in the summary and stored in `results.jsonl`.
