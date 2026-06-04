# base-benchmark

End-to-end throughput and latency benchmark for Base sequencer and validator nodes.

## Architecture

```
base-bench
  ├── PortManager           – allocates ephemeral ports per subprocess
  ├── SnapshotManager       – prepares (or caches) chain data dirs before each run
  ├── BaseRethNodeClient    – launches sequencer as a subprocess
  ├── BuilderClient         – wraps BaseRethNodeClient, adds flashblocks WS port
  ├── RPC Proxy (axum)      – intercepts eth_sendRawTransaction → FakeMempool
  ├── LoadTestPayloadWorker – spawns base-load-test, shares FakeMempool with proxy
  ├── SequencerConsensusClient – drives FCU→sleep→getPayload→newPayload loop
  ├── MetricsCollector      – scrapes Prometheus after each block
  ├── SyncingConsensusClient   – replays collected payloads through the validator
  └── NetworkBenchmark      – top-level orchestrator; writes per-run JSON results
```

The sequencer and validator each run as separate subprocesses. `base-bench` drives
block production via the Engine API (FCU v3, getPayload v4, newPayload v4).
Transactions are injected through the RPC proxy so the sequencer's txpool stays
under `base-bench`'s control. After the sequencer run, all collected payloads are
replayed through the validator to measure sync latency.

Binary subprocesses (`base-reth-node`, `base-load-test`) are discovered as siblings
of the `base-bench` executable, or supplied via flags. They are stopped with SIGINT;
if they don't exit within 5 seconds, SIGKILL is sent.

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

# Optional: enable flashblocks mode (builder node type only)
flashblocks:
  block_time_ms: 250

benchmarks:
  - node_type: reth        # "reth" or "builder"
    node_args: []          # extra args forwarded to the EL subprocess

    # {} = fresh in-memory devnet each run
    # Explicit paths = use an existing chain data directory (e.g. a ZFS snapshot)
    datadir: {}
    # datadir:
    #   sequencer: /home/user/snapshots/base-mainnet
    #   validator: /home/user/snapshots/base-mainnet-2

    # Optional: populate datadir by running a setup script before the first run.
    # The script is called with args: [node_type, output_dir].
    # Result is cached by sha256(command) so the script only runs once.
    # snapshot:
    #   command: ./scripts/setup.sh
    #   force_clean: false

    payload:
      id: my-payload
      type: load-test
      params:
        sender_count: 50
        funding_amount: "1000000000000000000"  # wei per sender
        transactions:
          - weight: 1
            type: uniswap_v3
            fee: 3000
            min_amount: "1000000000000"
            max_amount: "10000000000000"

    # Optional per-run metric thresholds. Violations are reported in the summary.
    # Error-severity violations cause base-bench to exit non-zero.
    metrics:
      warning:
        - metric: gas/per_block
          min: 1000000
      error:
        - metric: gas/per_second
          min: 100000000

    # Optional per-run tags merged into metadata.json
    tags:
      env: ci
```

### Transaction types

| `type` | Required fields | Notes |
|--------|----------------|-------|
| `transfer` | — | Simple ETH transfer |
| `calldata` | `max_size` | ETH transfer with random calldata |
| `erc20` | `contract` | ERC-20 transfer to a pre-deployed contract |
| `precompile` | `target`, `iterations` | Precompile call |
| `uniswap_v3` | `fee`, `min_amount`, `max_amount` | `router`/`token_in`/`token_out` auto-deployed if omitted |
| `aerodrome_cl` | `router`, `token_in`, `token_out`, `tick_spacing`, `min_amount`, `max_amount` | Aerodrome CL swap |

### Automatic Uniswap V3 setup

If a `uniswap_v3` entry has no `router` address, the runner deploys a `MockUniswapV3Router`
and two `FreeTransferERC20` tokens on the devnet before the load test starts, then injects
the addresses. Supply explicit addresses to use a real deployment (e.g. a mainnet snapshot).

### Matrix expansion

Add `variables` to sweep a parameter combinatorially across runs:

```yaml
benchmarks:
  - node_type: reth
    datadir: {}
    payload: ...
    variables:
      - name: sender_count
        values: ["10", "50", "100"]
```

Each combination becomes an independent run. Maximum 100 runs per invocation.

### Hardcoded accounts

All devnet runs use Hardhat well-known test accounts:

- **Batcher key** (account #0): `ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80`
- **Prefund key** (account #1): `59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d`
- **Prefund amount**: 1e24 wei (1,000,000 ETH)

These accounts are pre-funded in the devnet genesis. Do not use them on any public network.

## Output

Each run writes three files under `--output-dir/<run-id>/`:

| File | Contents |
|------|----------|
| `metadata.json` | Run config, git SHA/branch, tags, run group ID, per-metric averages |
| `metrics-sequencer.json` | Per-block metrics from the sequencer |
| `metrics-validator.json` | Per-block metrics from the validator |

A human-readable summary is also printed to stdout after each run.

### Metric names

Metrics are stored as flat key/value maps per block. The following names are always present:

| Metric | Source | Description |
|--------|--------|-------------|
| `gas/per_block` | sequencer + validator | Gas used in the block |
| `gas/per_second` | sequencer + validator | Gas used / block time |
| `transactions/per_block` | sequencer + validator | Transaction count |
| `latency/update_fork_choice` | sequencer + validator | `engine_forkchoiceUpdatedV3` round-trip (ns) |
| `latency/get_payload` | sequencer | `engine_getPayloadV4` round-trip (ns) |
| `latency/send_txs` | sequencer | Batch `eth_sendRawTransaction` round-trip (ns) |
| `latency/new_payload` | validator | `engine_newPayloadV4` round-trip (ns) |

Threshold metrics in the config (e.g. `gas/per_second`) reference these names.
