# Benchmark Tool — Rust Port Reference

This document captures every feature to port from the Go benchmark tool (`/home/meyer9/benchmark/`) into the Rust reimplementation (`/home/meyer9/base/`). Each section maps a feature to its Go source, describes the behavior to replicate, and notes any modifications for the simplified Rust port.

---

## Table of Contents

1. [CLI Layer](#1-cli-layer)
2. [YAML Config & Matrix Expansion](#2-yaml-config--matrix-expansion)
3. [Client Abstraction](#3-client-abstraction)
4. [Network Benchmark Orchestration](#4-network-benchmark-orchestration)
5. [Consensus Clients](#5-consensus-clients)
6. [Flashblocks](#6-flashblocks)
7. [Payload / Transaction Generation](#7-payload--transaction-generation)
8. [Metrics Collection](#8-metrics-collection)
9. [Output Directory Structure](#9-output-directory-structure)
10. [Infrastructure](#10-infrastructure)
11. [Rollup Config Generation](#11-rollup-config-generation)
12. [Dropped Features](#12-dropped-features)

---

## 1. CLI Layer

### Go Source
- `benchmark/cmd/main.go` — CLI entry point
- `benchmark/flags/flags.go` — flag definitions
- `benchmark/config/config.go` — CLI config wrapper
- `runner/config/config.go` — runner Config interface
- `runner/flags/flags.go` — client binary path flags

### Behavior

Single `run` command. Reads a YAML benchmark config, expands the test matrix, runs each benchmark, writes output.

### CLI Flags to Port

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--config` | string | (required) | Path to YAML benchmark config |
| `--root-dir` | string | `./data-dir` | Base directory for EL data dirs |
| `--output-dir` | string | `./output` | Base directory for benchmark output |
| `--load-test-bin` | string | `./base-load-test` | Path to load-test binary |
| `--benchmark-run-id` | string | (auto-generated) | Override the run ID |
| `--machine-type` | string | `""` | Machine type tag for output |
| `--machine-provider` | string | `""` | Machine provider tag |
| `--machine-region` | string | `""` | Machine region tag |
| `--file-system` | string | `ext4` | File system tag |
| `--reth-bin` | string | `reth` | Path to reth binary |
| `--geth-bin` | string | `geth` | Path to geth binary |
| `--builder-bin` | string | `builder` | Path to builder binary |
| `--base-reth-node-bin` | string | `base-reth-node` | Path to base-reth-node binary |

### Dropped Flags
- `--tx-fuzz-bin` (only load-test payload)
- `--proxy-port` (not needed)
- `--parallel-tx-batches` (moved to YAML config)
- Import-related flags entirely

### Rust Approach
- Use `clap` for CLI parsing
- Compile-time version via `env!("CARGO_PKG_VERSION")` or build script
- Env var prefix: `BASE_BENCH_`
- Use `tracing` + `tracing-subscriber` instead of op-service logging

---

## 2. YAML Config & Matrix Expansion

### Go Source
- `runner/benchmark/definition.go` — config types, YAML deserialization
- `runner/benchmark/matrix.go` — matrix expansion, ThresholdConfig
- `runner/benchmark/benchmark.go` — TestRun, RunParams, param parsing

### YAML Structure

```yaml
name: "benchmark-name"
description: "optional description"
block_time: "2s"         # default: 1s
parallel_tx_batches: 4   # NEW: moved from CLI flag
flashblocks:
  block_time: "250ms"    # default: 250ms

transaction_payloads:
  - id: "my-payload"
    type: "load-test"
    params:
      sender_count: 10
      funding_amount: "1000000000000000000"
      # transactions: optional, has defaults

benchmarks:
  - datadir:
      sequencer: "/path/to/sequencer/datadir"
      validator: "/path/to/validator/datadir"
    snapshot:
      command: "./scripts/setup.sh"
      genesis_file: "./genesis.json"   # required chain config
      force_clean: true
    metrics:
      warning:
        "gas/per_second": 50000000.0
      error:
        "gas/per_second": 30000000.0
    tags:
      env: "production"
    variables:
      - name: "node_type"
        values: ["reth", "geth"]
      - name: "gas_limit"
        values: ["60000000000"]
      - name: "num_blocks"
        value: "50"
```

### Param System

`NewParamsFromValues` (benchmark.go L58-L112) maps param names to RunParams fields:

| Param Name | RunParams Field | Type | Default |
|------------|----------------|------|---------|
| `node_type` | `NodeType` | string | `"geth"` |
| `validator_node_type` | `ValidatorNodeType` | string | same as `NodeType` |
| `gas_limit` | `GasLimit` | uint64 | `50_000_000_000` (50G) |
| `num_blocks` | `NumBlocks` | int | (from param) |
| `node_args` | `NodeArgs` | []string | space-separated or YAML list |
| `env` | `Env` | map[string]string | semicolon-separated `key=value` |
| `payload` | `PayloadID` | string | (from param) |

### Matrix Expansion

`ResolveTestRunsFromMatrix` (matrix.go L53-L116):
1. Collect all `Param` dimensions (each with `Value` or `Values`)
2. Validate no duplicate param types, total combinations <= 100
3. Compute cartesian product
4. Each combination becomes a `TestRun` with ID `test-<unix_microseconds>` and OutputDir `<id>-<index>`

### ThresholdConfig

```go
type ThresholdConfig struct {
    Warning map[string]float64
    Error   map[string]float64
}
```

Applied per-test to evaluate pass/warn/fail against collected metrics.

### Rust Approach
- Use `serde` + `serde_yaml` for deserialization
- Add `parallel_tx_batches` field to `BenchmarkConfig`
- Drop `SuperchainChainID` from snapshot definition
- Drop `ProofProgram` from test definition

---

## 3. Client Abstraction

### Go Source
- `runner/clients/interface.go` — Client enum, NewClient factory
- `runner/clients/types/types.go` — ExecutionClient trait, RuntimeConfig
- `runner/clients/reth/client.go` — Reth implementation
- `runner/clients/reth/metrics.go` — Reth Prometheus metrics
- `runner/clients/geth/client.go` — Geth implementation
- `runner/clients/geth/metrics.go` — Geth JSON metrics
- `runner/clients/builder/client.go` — Builder (wraps Reth + flashblocks port)
- `runner/clients/baserethnode/client.go` — BaseRethNode (variant Reth)
- `runner/clients/common/wait.go` — WaitForRPC helper
- `runner/config/client.go` — ClientOptions, InternalClientOptions

### ExecutionClient Trait

```go
type ExecutionClient interface {
    Run(ctx context.Context, cfg RuntimeConfig) error
    Stop() error
    Client() *ethclient.Client           // JSON-RPC client
    ClientURL() string                   // e.g. http://localhost:8545
    AuthClient() *rpc.Client             // Engine API client
    MetricsPort() uint64
    MetricsCollector() metrics.Collector
    GetVersion(ctx context.Context) (string, error)
    SetHead(ctx context.Context, blockNumber uint64) error
    FlashblocksClient() FlashblocksClient  // nil if N/A
    SupportsFlashblocks() bool
}
```

### RuntimeConfig

```go
type RuntimeConfig struct {
    Stdout, Stderr       io.WriteCloser
    Args                 []string
    FlashblocksURL       *string
    FlashblocksBlockTime string
    BlockTimeMs          uint64
}
```

### Client Implementations

#### Reth (`runner/clients/reth/client.go`)

Startup flow:
1. Acquire 4 ports: EL (HTTP RPC), AuthEL (Engine API), ELMetrics (Prometheus), P2P
2. Build args: `node --chain <chainCfg> --datadir <dataDir> --http --http.port <port> --http.api debug,net,eth --authrpc.port <authPort> --authrpc.jwtsecret <jwt> --metrics 0.0.0.0:<metricsPort> --engine.state-provider-metrics --disable-discovery --port <p2p> --txpool.max-pending-txs 100000 --txpool.max-new-txs 100000 --txpool.pending-max-count 100000 --txpool.pending-max-size 1073741824 --txpool.basefee-max-count 100000 --txpool.basefee-max-size 1073741824 --db.read-transaction-timeout 0 -vv`
3. Optional: `--flashblocks-url <url>` if flashblocks enabled
4. Append `cfg.Args` (user-provided node args)
5. Delete `<dataDir>/txpool/pending-transactions-backup` (reth-specific)
6. Read JWT secret from file
7. Start process, dial HTTP RPC (30s timeout), create ethclient
8. Create MetricsCollector
9. WaitForRPC (poll `client.BlockNumber()` every 1s, up to 240s)
10. Dial auth RPC (240s timeout)

Stop: SIGINT → wait (5s deadline) → release 4 ports

Metrics collection (reth/metrics.go): Scrapes Prometheus text format from `http://localhost:{metricsPort}/metrics`. Tracked metrics:
- `reth_sync_execution_execution_duration` (histogram)
- `reth_sync_block_validation_state_root_duration` (histogram)
- `reth_sync_state_provider_*_fetch_latency` — 6 variants: storage, account, code, storage_total, account_total, code_total (histograms)

#### Geth (`runner/clients/geth/client.go`)

Differences from Reth:
1. Runs `geth init --state.scheme hash --datadir <dataDir> <chainCfg>` first (unless SkipInit)
2. Args: `--state.scheme hash --syncmode full --gcmode archive --http --http.port <port> --http.api debug,net,eth --authrpc.port <authPort> --authrpc.jwtsecret <jwt> --metrics --metrics.addr 0.0.0.0 --metrics.port <metricsPort> --maxpeers 0 --nodiscover --port <p2pPort> --rpc.txfeecap 20 --miner.newpayload-timeout 2s -vv`
3. Auth dial timeout: 30s (vs 240s for reth)
4. `SupportsFlashblocks = false`

Metrics collection (geth/metrics.go): Reads JSON from `http://127.0.0.1:{metricsPort}/debug/metrics`. Tracked metrics (all `.50-percentile`):
- `chain/account/reads`, `chain/account/updates`, `chain/account/hashes`, `chain/account/commits`
- `chain/storage/reads`, `chain/storage/updates`, `chain/storage/commits`
- `chain/execution`, `chain/crossvalidation`, `chain/validation`, `chain/write`
- `chain/snapshot/commits`, `chain/triedb/commits`, `chain/inserts`

#### Builder (`runner/clients/builder/client.go`)

Wraps a Reth client (uses builder binary). Additions:
1. Allocates an extra `FlashblocksWebsocketPortPurpose` port
2. Appends `--flashblocks.port <wsPort> --flashblocks.block-time <blockTime> --rollup.chain-block-time <blockTimeMs>ms` to args
3. Creates a `FlashblocksClient` (WebSocket client connecting to `ws://localhost:{wsPort}`)
4. `SupportsFlashblocks = false` (it *produces* flashblocks, doesn't receive them)

#### BaseRethNode (`runner/clients/baserethnode/client.go`)

Similar to Reth but:
1. Uses `base-reth-node` binary
2. Uses `--websocket-url` for flashblocks (vs `--flashblocks-url` for reth)
3. Verbosity `-vvv` (vs `-vv` for reth)
4. `SupportsFlashblocks = true` (receives flashblocks via WebSocket)

### ClientOptions

```go
type ClientOptions struct {
    CommonOptions          // NodeArgs []string
    RethOptions            // RethBin string
    GethOptions            // GethBin string
    BuilderOptions         // BuilderBin string
    BaseRethNodeOptions    // BaseRethNodeBin string
}

type InternalClientOptions struct {
    JWTSecretPath  string
    ChainCfgPath   string
    DataDirPath    string
    TestDirPath    string
    JWTSecret      string
    MetricsPath    string
}
```

### Rust Approach
- Define `ExecutionClient` trait with async methods
- Use `tokio::process::Command` for subprocess management
- Use `reqwest` for HTTP-based metrics scraping
- Use `alloy` for Ethereum JSON-RPC and Engine API
- Port all 4 client types

---

## 4. Network Benchmark Orchestration

### Go Source
- `runner/network/network_benchmark.go` — top-level orchestrator
- `runner/service.go` — service lifecycle (L103-L250 for runTest flow)

### NetworkBenchmark

```go
type NetworkBenchmark struct {
    log                    log.Logger
    sequencerOptions       config.InternalClientOptions
    validatorOptions       config.InternalClientOptions
    collectedSequencerMetrics *metrics.FileMetricsWriter
    collectedValidatorMetrics *metrics.FileMetricsWriter
    testConfig             *types.TestConfig
    transactionPayload     payload.Definition
    ports                  benchmark.PortManager
    flashblocksBlockTime   time.Duration
}
```

### Run Flow

1. Call `benchmarkSequencer()`:
   - `setupNode()` for sequencer (creates client, starts process, connects RPC)
   - Create `FileMetricsWriter` for sequencer
   - Run `sequencerBenchmark.Run()` → returns `PayloadResult` (executable payloads + flashblocks)
   - Defer: stop client, collect metrics, close metrics writer
2. Call `benchmarkValidator(payloads)`:
   - If flashblocks present: start `ReplayServer` on WebSocket port
   - `setupNode()` for validator (pass flashblocks URL if applicable)
   - If validator head is behind sequencer first payload: catch up via `engine_newPayloadV4` + `engine_forkchoiceUpdatedV3`
   - Create `FileMetricsWriter` for validator
   - Run `validatorBenchmark.Run()` with collected payloads
   - Defer: stop client, stop replay server, collect metrics
3. Return `RunResult{SequencerMetrics, ValidatorMetrics, ClientVersion}`

### setupNode (network_benchmark.go)

1. Map `node_type` string → `Client` enum (reth/geth/builder/base-reth-node)
2. `clients.NewClient(clientType, log, clientOptions, portManager)`
3. Create log file at `<testDir>/<nodeType>-el.log`
4. Build `MultiWriterCloser` for stdout/stderr (structured logger + file)
5. Build `RuntimeConfig` with flashblocks URL, block time
6. Call `client.Run(ctx, runtimeConfig)`

### TestConfig

```go
type TestConfig struct {
    Params           RunParams
    Config           config.Config
    Genesis          core.Genesis
    BatcherKey       *ecdsa.PrivateKey
    PrefundPrivateKey *ecdsa.PrivateKey
    PrefundAmount    *big.Int
}
```

Batcher and prefund keys are hardcoded (from hex constants in service.go).

### Rust Approach
- Mirror the struct hierarchy
- Use `tokio` async runtime for concurrent operations
- Hardcode the same test keys

---

## 5. Consensus Clients

### Go Source
- `runner/network/consensus/client.go` — base consensus client
- `runner/network/consensus/sequencer_consensus.go` — sequencer consensus
- `runner/network/consensus/validator_consensus.go` — validator consensus
- `runner/network/mempool/fake_mempool.go` — transaction mempool

### BaseConsensusClient

Core Engine API wrapper:
- `updateForkChoice(payloadAttributes)` → `engine_forkchoiceUpdatedV3` (10s timeout)
- `getBuiltPayload(payloadID)` → `engine_getPayloadV4` (240s timeout)
- `newPayload(executableData, beaconRoot)` → `engine_newPayloadV4` (30s timeout) with empty blob hashes + empty execution requests hash
- Tracks `headBlockHash`, `headBlockNumber`, `currentPayloadID`

### SequencerConsensusClient

Options: `BlockTime`, `GasLimit`, `GasLimitSetup` (1B gas for setup blocks), `ParallelTxBatches`

`Propose()` flow:
1. Get pending txs from mempool via `NextBlock()`
2. Send txs in parallel batches: split into groups of 100, send N parallel `eth_sendRawTransaction` batch calls
3. Call `updateForkChoice` with payload attributes:
   - Timestamp incremented by block time
   - L1BlockInfo deposit tx (Jovian format via rollup config)
   - EIP-1559 params: Holocene encoding (elasticity=50, denominator=1), MinBaseFee=1
   - GasLimit from config
   - `NoTxPool = false`
4. Sleep for block time
5. `getBuiltPayload` → executable data
6. Record metrics:
   - `latency/update_fork_choice` — FCU call duration
   - `latency/get_payload` — getPayload call duration
   - `latency/send_txs` — batch send duration
   - `gas/per_block` — gas used in block
   - `gas/per_second` — gas used / block time
   - `transactions/per_block` — tx count in block
7. `newPayload` with the executable data
8. Return executable payloads collected across all blocks

### SyncingConsensusClient (Validator)

`Start(payloads, metricsCollector, firstTestBlock, startedBlockSignal)`:
1. Iterate through executable payloads
2. For each: `newPayload` → `updateForkChoice(nil)` (no new payload attributes)
3. Record:
   - `latency/new_payload` — newPayload call duration
   - `latency/update_fork_choice` — FCU call duration
   - `gas/per_block`, `gas/per_second`, `transactions/per_block`
4. Sleep remaining block time
5. Collect metrics only for blocks >= `firstTestBlock`
6. Send block signal on channel

### FakeMempool

`StaticWorkloadMempool`:
- Thread-safe (mutex)
- `AddTransactions(txs)`: encodes each tx via `EncodeTx()`, separates `DepositTxType` into `sequencerTxs` vs normal `sendTxs`
- `NextBlock()`: returns `(sendTxs, sequencerTxs)`, then clears both lists
- Tracks per-address nonce for `IsthmusSigner` (chain-ID aware)

### Rust Approach
- Use `alloy` transport for Engine API calls
- Port the parallel tx batch sending with `tokio::spawn`
- Generate L1BlockInfo deposit transactions matching Jovian format

---

## 6. Flashblocks

### Go Source
- `runner/clients/types/flashblock.go` — flashblock payload types
- `runner/clients/types/flashblocks_client.go` — FlashblocksClient interface
- `runner/clients/builder/flashblocks_client.go` — WebSocket client impl
- `runner/network/flashblocks/replay_server.go` — WebSocket replay server

### Types

```go
type FlashblocksPayloadV1 struct {
    PayloadID [8]byte
    Index     uint64
    Base      *ExecutionPayloadBaseV1       // first flashblock has this
    Diff      ExecutionPayloadFlashblockDeltaV1
    Metadata  json.RawMessage
}
```

### FlashblocksClient (WebSocket Consumer)

Interface: `Start(ctx)`, `Stop()`, `AddListener(FlashblockListener)`, `RemoveListener()`, `IsConnected()`
Listener callback: `OnFlashblock(FlashblocksPayloadV1)`

Used by Builder client to receive flashblocks from the builder's `--flashblocks.port`.

### FlashblockReplayServer (WebSocket Producer)

Replays collected flashblocks to the validator:
1. Starts HTTP server with WebSocket upgrade handler on a port
2. `WaitForConnection(timeout)` blocks until a client connects
3. `ReplayFlashblock(blockNumber)`: sends flashblocks at evenly-spaced intervals within block time as binary WebSocket messages
4. `Stop()`: sends close message, shuts down server

### Rust Approach
- Use `tokio-tungstenite` for WebSocket client/server
- Port the replay timing logic (evenly spaced within block time)

---

## 7. Payload / Transaction Generation

### Go Source
- `runner/payload/worker/types.go` — Worker interface
- `runner/payload/factory.go` — payload factory
- `runner/payload/loadtest/load_test_worker.go` — load-test implementation

### Worker Interface

```go
type Worker interface {
    Setup(ctx context.Context) error
    SendTxs(ctx context.Context, pendingTxs int) (int, error)
    Stop(ctx context.Context) error
    Mempool() mempool.FakeMempool
}
```

### Load-Test Worker (sole payload type)

`LoadTestPayloadWorker`:
1. **Setup**: starts a proxy server that intercepts ETH RPC calls, writes a temp YAML config for the load-test binary:
   ```yaml
   rpc: <proxy_url>
   sender_count: <from params>
   target_gps: <gas_limit / block_time_seconds>
   duration: "99999s"
   seed: <crypto_random_uint64>
   funding_amount: <from params>
   transactions: <from params or defaults>
   ```
2. Starts `base-load-test` binary with env `FUNDER_KEY=<prefund_private_key>`
3. **SendTxs**: collects pending transactions from proxy, adds to mempool. Returns count.
4. **Default transactions** (if not specified):
   ```yaml
   - type: transfer
     weight: 70
   - type: calldata
     weight: 20
     max_size: 256
   - type: precompile
     weight: 10
     name: sha256
   ```

### LoadTestPayloadDefinition

```go
type LoadTestPayloadDefinition struct {
    SenderCount    int       `yaml:"sender_count"`
    FundingAmount  string    `yaml:"funding_amount"`
    Transactions   yaml.Node `yaml:"transactions"`  // pass-through to load-test binary
}
```

### Rust Approach
- Port the proxy server pattern (intercept RPC to capture txs)
- Use `tokio::process::Command` to spawn load-test binary
- Only implement load-test worker

---

## 8. Metrics Collection

### Go Source
- `runner/metrics/metrics_interface.go` — Collector, BlockMetrics, FileMetricsWriter

### Collector Interface

```go
type Collector interface {
    Collect(ctx context.Context, blockMetrics *BlockMetrics) error
    GetMetrics() []BlockMetrics
}
```

### BlockMetrics

```go
type BlockMetrics struct {
    BlockNumber      uint64
    Timestamp        time.Time
    prevMetrics      map[string]float64          // for delta calculations
    ExecutionMetrics map[string]interface{}
}
```

Methods:
- `UpdatePrometheusMetric(name, metricFamily)` — handles histogram (sum), gauge, counter (delta from prev), summary (sum). For counters, stores prev value and reports delta.
- `AddExecutionMetric(name, value)` — sets key/value in ExecutionMetrics
- `GetMetricFloat(name)` — retrieves as float64

### FileMetricsWriter

Writes `metrics.json` to a base directory. Collects BlockMetrics from the Collector after benchmark completes.

### Key Metrics Names

Sequencer:
- `latency/update_fork_choice` — FCU call duration (seconds)
- `latency/get_payload` — getPayload call duration
- `latency/send_txs` — batch tx send duration
- `gas/per_block` — gas used in block
- `gas/per_second` — gas used / block time
- `transactions/per_block` — tx count in block

Validator:
- `latency/new_payload` — newPayload call duration
- `latency/update_fork_choice` — FCU call duration
- `gas/per_block`, `gas/per_second`, `transactions/per_block`

Client-specific (reth):
- `reth_sync_execution_execution_duration`
- `reth_sync_block_validation_state_root_duration`
- `reth_sync_state_provider_{storage,account,code}_fetch_latency`
- `reth_sync_state_provider_{storage,account,code}_total_fetch_latency`

Client-specific (geth):
- `chain/{account,storage}/{reads,updates,hashes,commits}.50-percentile`
- `chain/{execution,crossvalidation,validation,write,inserts}.50-percentile`
- `chain/{snapshot,triedb}/commits.50-percentile`

Flashblock-specific (reth):
- `reth_flashblocks_*` — various flashblock metrics from Prometheus

### Rust Approach
- Port the delta-calculation logic for counter metrics
- Use `reqwest` for HTTP metrics scraping
- Parse Prometheus text format (reth) and JSON (geth)

---

## 9. Output Directory Structure

### Go Source
- `runner/service.go` — `exportOutput()`, `runTest()`

### Per-Run Output

Each test run creates: `<output-dir>/<run-id>/<test-id>-<index>/`

Contents:
- `metrics-<node_type>.json` — collected BlockMetrics array
- `result-<node_type>.json` — RunResult summary (success, client version, metric summaries)
- `logs-<node_type>-el.log.gz` — gzipped EL node stdout/stderr (max 1MB dump on failure)
- `tags.json` — **NEW**: tags from test definition + machine info

### Rust Approach
- Replicate directory structure
- Use `flate2` for gzip compression
- Write tags as separate JSON file per run

---

## 10. Infrastructure

### Go Source
- `runner/service.go` — service lifecycle
- `runner/benchmark/portmanager/ports.go` — port management
- `runner/benchmark/snapshots.go` — snapshot/datadir management
- `runner/logger/logger.go` — LogWriter
- `runner/logger/multi.go` — MultiWriterCloser
- `runner/utils/id.go` — random ID generation

### Service Lifecycle (`runner/service.go`)

`Run()` flow:
1. Read YAML config from `--config` path
2. For each benchmark in config: `NewTestPlanFromConfig()` → `TestPlan`
3. Ensure output directory exists
4. Generate or use provided `BenchmarkRunID`
5. For each test plan, for each run:
   a. Create output directory `<output>/<runId>/<testRun.OutputDir>/`
   b. Call `runTest(testPlan, testRun)`
   c. Record result (success/failure)

`runTest()` flow:
1. Get genesis: from snapshot definition (run command or read genesis file) or default devnet genesis
2. Setup sequencer data dir: create test dir, metrics dir, write chain config, handle snapshot restore, generate JWT secret
3. Setup validator data dir: same process
4. Create `TestConfig` with params, genesis, hardcoded keys:
   - Batcher key: `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80`
   - Prefund key: `0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d`
   - Prefund amount: `1e24` wei
5. Create `NetworkBenchmark` and run it
6. Export output: move metrics.json, gzip logs, write result JSON

### Port Management (`portmanager/ports.go`)

```go
type PortPurpose int
const (
    ELPortPurpose                    // HTTP JSON-RPC
    AuthELPortPurpose                // Engine API
    ELMetricsPortPurpose             // Prometheus/JSON metrics endpoint
    BuilderMetricsPortPurpose        // Builder metrics
    P2PPortPurpose                   // P2P port
    FlashblocksWebsocketPortPurpose  // WebSocket for flashblocks
)
```

`PortManager`:
- `AcquirePort(nodeType, purpose)`: scans from 10000-65535, finds port that is (a) not already tracked and (b) not currently listening (checked via `net.Listen`). Returns port.
- `ReleasePort(port)`: removes from tracked set
- Thread-safe via mutex

### Snapshot Management (`snapshots.go`)

`SnapshotManager`:
- `EnsureSnapshot(datadirsConfig, definition, nodeType, role)`:
  1. Check cache by `{nodeType, role, command}` key
  2. Resolve storage path from `DatadirConfig` (sequencer/validator specific) or generate hash-based path
  3. If `ForceClean` or path doesn't exist: run `definition.CreateSnapshot(nodeType, outputDir)`
  4. `CreateSnapshot()` executes the command with args `[nodeType, outputDir]`
  5. Cache the result path

### Logger (`logger/`)

`LogWriter`: wraps `log.Logger`, buffers up to 16KB per line, escapes non-printable characters, flushes on newline. Implements `io.WriteCloser`.

`MultiWriterCloser`: fans out writes to multiple `io.WriteCloser`s (e.g., structured logger + file). Close closes all.

### Random ID Generation (`utils/id.go`)

`GenerateRandomID(bytes)`: `crypto/rand` → hex-encoded string.

### Rust Approach
- Port manager: use `std::net::TcpListener::bind()` to check port availability
- Snapshot manager: use `tokio::process::Command` for setup scripts
- Logger: use `tracing` with a file appender layer
- Random IDs: use `rand` crate with hex encoding

---

## 11. Rollup Config Generation

### Go Source
- `runner/network/configutil/rollup_config.go`

### GetRollupConfig

Builds a `rollup.Config` from genesis + chain config + batcher address + block time:

Key hardcoded values:
- `MaxSequencerDrift`: 20
- `SeqWindowSize`: 24
- `L1ChainID`: 1
- `BatchInboxAddress`: `0x0000000000000000000000000000000000000001`
- All OP fork times read from genesis config (Regolith through Interop), set to 0 if present
- Holocene EIP-1559 params: elasticity=50, denominator=1
- Genesis block IDs from L1 block 0 and L2 genesis block

Used by `SequencerConsensusClient.generatePayloadAttributes()` to build the L1BlockInfo deposit transaction in each proposed block.

### Rust Approach
- Port the rollup config construction using `op-alloy` types
- Replicate the L1BlockInfo deposit tx encoding (Jovian format)

---

## 12. Dropped Features

These features from the Go codebase are **not** being ported:

| Feature | Go Source | Reason |
|---------|-----------|--------|
| `import-runs` command | `benchmark/cmd/main.go`, `runner/importer/` | Not needed |
| Importer service | `runner/importer/` (entire package) | Not needed |
| Result metadata (metadata.json) | `runner/benchmark/result_metadata.go` | Dynamically calculated by metrics site |
| tx-fuzz payload | `runner/payload/txfuzz/` | Only load-test needed |
| transfer-only payload | `runner/payload/transferonly/` | Only load-test needed |
| contract payload | `runner/payload/contract/` | Only load-test needed |
| simulator payload | `runner/payload/simulator/` | Only load-test needed |
| Fault proof benchmark | `runner/network/fault_proof_benchmark.go` | Not needed |
| L1 chain (fake) | `runner/network/l1_chain.go` | Only used by FPP |
| op-program integration | `runner/network/proofprogram/` | Not needed |
| Superchain registry | Snapshot `SuperchainChainID` field | Not needed (chain config required) |
| Port overrides | `config.PortOverrides` | Automatic assignment only |
| op-service logging | `benchmark/cmd/main.go` | Replace with tracing |
| Proxy port flag | `--proxy-port` | Not needed |
| tx-fuzz binary flag | `--tx-fuzz-bin` | Not needed |
| `parallel-tx-batches` CLI flag | `benchmark/flags/flags.go` | Moved to YAML config |
