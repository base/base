---
status: in-progress
phase: 3
updated: 2026-05-04
---

# Implementation Plan: base-benchmark Crate

## Goal
Port the Go benchmark tool to a new `base-benchmark` Rust crate at `crates/infra/benchmark/` that orchestrates EL clients (BaseRethNode, Builder), drives consensus via Engine API, collects metrics, and invokes `base-load-test` as the transaction payload worker.

## Finalized Decisions

| Decision | Detail | Source |
|----------|--------|--------|
| Crate name / binary | `base-benchmark` at `crates/infra/benchmark/`, binary `base-bench` | `ref:bg_a1c53346` — workspace naming convention |
| Engine API | Reuse `BaseEngineClient` from `base-consensus-engine` (FCU v3, getPayload v4, newPayload v4, JWT auth). Generic over `<L1Provider, L2Provider>`, requires `l1_rpc` URL — use dummy endpoint. | `ref:bg_05b4ee56`, `ref:bg_32780db9` |
| JWT | Reuse `base-jwt` (`JwtSecretReader::write_to_path`, `default_jwt_secret`) | `ref:bg_a1c53346` |
| Genesis / rollup config | Reuse `base-common-genesis` (`RollupConfig`, `ChainGenesis`, `HardForkConfig`) and `base-test-utils::build_test_genesis()` | `ref:bg_a1c53346` |
| Flashblocks types | Reuse `FlashblocksPayloadV1`, `ExecutionPayloadFlashblockDeltaV1` from `base-common-flashblocks` — do NOT reimplement | `ref:bg_32780db9` |
| Deposit tx types | Reuse `TxDeposit`, `DEPOSIT_TX_TYPE_ID` (0x7E) from `base-common-consensus` | `ref:bg_32780db9` |
| L1BlockInfo | Reuse `L1BlockInfo` from `base-common-evm` | `ref:bg_32780db9` |
| Rollup params | `src/params.rs` with Rust consts (no YAML params file) | user decision |
| Binary location | `--reth-bin` / `--builder-bin` / `--load-test-bin` CLI flags; default = sibling of current exe (`std::env::current_exe().parent()`) | user decision |
| Proxy architecture | Axum-based JSON-RPC proxy intercepting `eth_sendRawTransaction`, matching `ingress-rpc` crate patterns | `ref:bg_7b0383d2` — workspace uses axum + jsonrpsee |
| Flashblocks replay | Axum WebSocket server with broadcast channel, matching `websocket-proxy` crate patterns | `ref:bg_7b0383d2` |
| Funding | Keep DepositTx injection (0x7E, From=Address{1}) for mainnet snapshot support | user decision |
| L1 chain | No real L1; fake zero L1BlockInfo via `L1BlockInfo::default()`. `EngineClientBuilder` gets dummy `l1_rpc` URL | user decision, `ref:bg_32780db9` |
| Clients in scope | BaseRethNode + Builder only | user decision |
| Snapshot | External script with `[node_type, snapshot_path]` args; cache by `sha256(command)[:12]`; `force_clean` flag | user decision |
| Flashblocks | In scope: WS consumer + replay server | user decision |
| Duration | No duration field; run exactly `num_blocks`. Subprocess gets `duration=99999s`, killed via `Stop()` | user decision |
| Process signals | `nix::sys::signal::kill(Pid, SIGINT)` for graceful shutdown, 5s timeout, escalate to `SIGKILL` | `ref:bg_17a5e958` — Go uses SIGINT + WaitDelay |

## Cross-Cutting Conventions

- **lib.rs**: Minimal, `#![doc = include_str!("../README.md")]`, grouped `mod foo; pub use foo::*;`
- **mod.rs**: Every `mod.rs` begins with `//!` doc comment
- **Visibility**: All structs/enums/functions `pub`, re-exported from `lib.rs`. No `pub mod` except test utils.
- **Tracing**: Structured key=value fields only. `info!(block = %n, gas = %g, "block proposed")`. Never interpolated strings.
- **Tests**: `#[cfg(test)] mod tests { ... }` at end of every file with testable logic
- **Cargo.toml**: Deps sorted by line length (waterfall). `[lints] workspace = true`. Features at bottom.
- **Errors**: Crate-level `BenchmarkError` enum via `thiserror`. All functions return `Result<T, BenchmarkError>` or `eyre::Result<T>`.
- **Binary**: `[[bin]] name = "base-bench" path = "src/bin/base_bench.rs"` explicit mapping

## Module Layout

```
crates/infra/benchmark/
├── Cargo.toml
├── README.md
├── PORTING_REFERENCE.md
├── PLAN.md
├── examples/
│   ├── devnet.yaml
│   └── snapshot.sh
└── src/
    ├── lib.rs
    ├── error.rs            # BenchmarkError enum
    ├── params.rs           # Const rollup/chain params
    ├── config/
    │   └── mod.rs          # BenchmarkConfig, BenchmarkDefinition, matrix expansion
    ├── ports.rs            # PortManager
    ├── process.rs          # ProcessHandle (spawn, SIGINT/SIGKILL, log capture)
    ├── snapshots.rs        # SnapshotManager
    ├── output.rs           # Output dir, gzip, result JSON, RandomId
    ├── client/
    │   └── mod.rs          # ExecutionClient trait, BaseRethNodeClient, BuilderClient
    ├── consensus/
    │   └── mod.rs          # BaseConsensusClient, Sequencer, Syncing, FakeMempool
    ├── payload/
    │   └── mod.rs          # Worker trait, LoadTestPayloadWorker
    ├── proxy.rs            # Axum JSON-RPC proxy
    ├── metrics/
    │   └── mod.rs          # BlockMetrics, PrometheusCollector, FileMetricsWriter
    ├── flashblocks/
    │   └── mod.rs          # FlashblocksClient (WS consumer), FlashblockReplayServer
    ├── rollup.rs           # BenchmarkRollupConfig::build() method
    ├── runner/
    │   └── mod.rs          # TestConfig, NetworkBenchmark, sequencer/validator flows
    ├── service.rs          # setup_internal_directories, run_test, export_output, main loop
    └── bin/
        └── base_bench.rs   # CLI glue only
```

> **AGENTS.md rules applied:** Each `mod foo; pub use foo::*;` grouped together in lib.rs. No `pub mod` except test utils. All types `pub`. No logic in lib.rs. Binary contains only glue. `mod.rs` files begin with `//!` doc comment. Structured tracing with key=value fields.

## Phase 1: Crate Scaffold, Config & Error Types [COMPLETE]

- [ ] 1.1 Create `Cargo.toml`: workspace deps (`clap`, `tokio`, `serde`, `serde_yaml`, `serde_json`, `tracing`, `tracing-subscriber`, `rand`, `thiserror`, `eyre`, `alloy`, `reqwest`, `axum`, `tokio-tungstenite`, `flate2`, `sha2`, `nix`, `prometheus-parse`, `base-consensus-engine`, `base-jwt`, `base-common-genesis`, `base-common-flashblocks`, `base-common-consensus`, `base-common-evm`, `base-test-utils`). `[[bin]] name = "base-bench" path = "src/bin/base_bench.rs"`. `[lints] workspace = true`.
- [ ] 1.2 Create `src/error.rs`: `BenchmarkError` enum with variants: `Config(String)`, `Io(#[from] std::io::Error)`, `Client(String)`, `EngineApi(String)`, `Metrics(String)`, `Proxy(String)`, `Snapshot(String)`, `Timeout(String)`, `ProcessCrash { binary: String, exit_code: Option<i32> }`. All Display via thiserror.
- [ ] 1.3 Create minimal `src/lib.rs` and stub `src/bin/base_bench.rs`
- [ ] 1.4 Add `"crates/infra/benchmark"` to workspace `Cargo.toml` members
- [ ] 1.5 Define config types in `src/config/mod.rs`: `BenchmarkConfig` {name, description, block_time_ms, num_blocks, parallel_tx_batches, flashblocks: Option<FlashblocksConfig>, transaction_payloads: Vec<TransactionPayloadDef>, benchmarks: Vec<BenchmarkDefinition>}. `BenchmarkDefinition` {datadir: DatadirConfig, snapshot: Option<SnapshotConfig>, metrics: Option<MetricsConfig>, tags: HashMap<String, String>, variables: Vec<Variable>}. `DatadirConfig` {sequencer: Option<PathBuf>, validator: Option<PathBuf>}. `SnapshotConfig` {command: String, genesis_file: Option<PathBuf>, force_clean: bool}. `MetricsConfig` {warning: Vec<MetricsThreshold>, error: Vec<MetricsThreshold>}. `MetricsThreshold` {metric: String, min: Option<f64>, max: Option<f64>}. `Variable` {name: String, values: Vec<String>}. `TransactionPayloadDef` {id: String, payload_type: String, params: LoadTestPayloadParams}. `LoadTestPayloadParams` {sender_count: u64, funding_amount: U256, transactions: Vec<WeightedTx>}. `FlashblocksConfig` {block_time_ms: u64}. `TestRun` {id: String, params: HashMap<String, String>, definition: BenchmarkDefinition, payload: TransactionPayloadDef}. All `Deserialize` + `Serialize`.
- [ ] 1.6 Create `src/params.rs`: `pub const MAX_SEQUENCER_DRIFT: u64 = 20`, `SEQ_WINDOW_SIZE: u64 = 24`, `L1_CHAIN_ID: u64 = 1`, `BATCH_INBOX_ADDRESS: Address = address!("...")`, `EIP1559_ELASTICITY: u64 = 50`, `EIP1559_DENOMINATOR: u64 = 1`, `BATCHER_KEY: B256 = b256!("...")`, `PREFUND_KEY: B256 = b256!("...")`, `PREFUND_AMOUNT: U256 = ...`, `SUGGESTED_FEE_RECIPIENT: Address = address!("4200000000000000000000000000000000000011")`, `DEFAULT_GAS_LIMIT: u64 = 30_000_000`, `SETUP_GAS_LIMIT: u64 = 1_000_000_000`.
- [ ] 1.7 Implement matrix expansion in `src/config/mod.rs`: `BenchmarkConfig::expand(&self) -> Result<Vec<TestRun>, BenchmarkError>`. Cartesian product of `variables` arrays. If result > 100 runs, return `Err(BenchmarkError::Config("matrix expansion exceeds 100 test runs".into()))`. Each `TestRun` gets a unique ID via `random_id()`.
- [ ] 1.8 Implement CLI in `src/bin/base_bench.rs`: `#[derive(Parser)] struct Cli` with `--config: PathBuf`, `--root-dir: PathBuf`, `--output-dir: PathBuf`, `--benchmark-run-id: Option<String>`, `--reth-bin: Option<PathBuf>` (default: sibling exe / "base-reth-node"), `--builder-bin: Option<PathBuf>` (default: sibling exe / "base-builder"), `--load-test-bin: Option<PathBuf>` (default: sibling exe / "base-load-test"), `--machine-type: Option<String>`, `--machine-provider: Option<String>`, `--machine-region: Option<String>`, `--file-system: Option<String>`. Env: `#[arg(env = "BASE_BENCH_CONFIG")]` etc.
- [ ] 1.9 Tests: config deserialization round-trip, matrix expansion (0 vars, 1 var, 2+ vars, >100 error), params const sanity checks.

## Phase 2: Infrastructure Utilities [COMPLETE]

- [x] 2.1 `PortManager` in `src/ports.rs` — acquire/release/acquire_n, TcpListener::bind probe
- [x] 2.2 `ProcessHandle` in `src/process.rs` — SIGINT + 5s grace + SIGKILL, structured tracing
- [x] 2.3 `SnapshotManager` in `src/snapshots.rs` — sha256(command)[:12] cache, force_clean, explicit path passthrough
- [x] 2.4 Output helpers in `src/output.rs` — gzip, write_result_json, dump_log_tail, random_id (shipped in Phase 1)
- [x] 2.5 Tests — 20 tests total passing (ports ×5, snapshots ×6, output ×2, config ×4, matrix ×3)

## Phase 3: Client Abstraction [PENDING]

- [ ] 3.1 Define `ExecutionClient` trait in `src/client/mod.rs`:
  ```rust
  #[async_trait]
  pub trait ExecutionClient: Send + Sync {
      async fn run(&mut self) -> Result<(), BenchmarkError>;
      async fn stop(&mut self) -> Result<(), BenchmarkError>;
      fn rpc_url(&self) -> &str;
      fn auth_rpc_url(&self) -> &str;
      fn metrics_port(&self) -> u16;
      async fn get_version(&self) -> Result<String, BenchmarkError>;
      async fn set_head(&self, block: u64) -> Result<(), BenchmarkError>;
      fn flashblocks_client(&self) -> Option<&FlashblocksClient>;
      fn supports_flashblocks(&self) -> bool;
  }
  ```
  Define `ClientOptions` {node_type: String, extra_args: Vec<String>, reth_bin: PathBuf, builder_bin: PathBuf}. Define `InternalClientOptions` {jwt_secret_path: PathBuf, chain_cfg_path: PathBuf, data_dir_path: PathBuf, test_dir_path: PathBuf, jwt_secret: [u8; 32], metrics_dir: PathBuf}.
- [ ] 3.2 Implement `BaseRethNodeClient` in `src/client/mod.rs`: holds `ProcessHandle`, `Arc<PortManager>`, 4 acquired ports (EL, AuthEL, ELMetrics, P2P), `InternalClientOptions`. `run()`: delete `data_dir/txpool-transactions-backup.rlp` if exists; build arg vec (--color never --chain <chain_cfg_path> --datadir <data_dir> --http --http.port <el_port> --http.api eth,net,web3,miner --authrpc.port <auth_port> --authrpc.jwtsecret <jwt_path> --metrics <metrics_port> --engine.state-provider-metrics --disable-discovery --port <p2p_port> -vvv --txpool.pending-max-count 100000000 --txpool.queued-max-count 100000000 --txpool.max-account-slots 100000000 --txpool.pending-max-size 100 --txpool.queued-max-size 100 --db.read-transaction-timeout 0 [extra_args] [--websocket-url <fb_url> if Some]); spawn via ProcessHandle; poll `eth_chainId` until success (240s, 500ms interval); dial auth RPC with JWT. `stop()`: ProcessHandle.stop(), release 4 ports. `get_version()`: run `binary --version`, parse "Version: X.Y.Z". `set_head(block)`: call `debug_setHead` RPC.
- [ ] 3.3 Implement `BuilderClient` in `src/client/mod.rs`: wraps `BaseRethNodeClient`. `run()`: acquire FlashblocksWebsocket port, append `--flashblocks.port <ws_port> --flashblocks.block-time <fb_ms> --rollup.chain-block-time <block_ms>` to inner args, call inner `run()`, create `FlashblocksClient(ws_port)`. `stop()`: stop flashblocks client, release ws port, call inner `stop()`. `supports_flashblocks()` = false. `flashblocks_client()` returns the WS client handle.
- [ ] 3.4 Implement `setup_node()` fn: match `node_type` str → `Box<dyn ExecutionClient>` ("base-reth-node" → `BaseRethNodeClient`, "builder" → `BuilderClient`), open `el.log` in test dir, call `client.run()`, return boxed client.
- [ ] 3.5 Tests: BaseRethNodeClient arg list construction. BuilderClient extra arg appending.

## Phase 4: Consensus Clients [PENDING]

- [ ] 4.1 Implement `BaseConsensusClient` in `src/consensus/mod.rs`: wraps `BaseEngineClient<RootProvider, RootProvider<Base>>` built via `EngineClientBuilder { l2: auth_url, l2_jwt, l1_rpc: Url::parse("http://127.0.0.1:1").unwrap(), cfg }`. Fields: `head_block_hash: B256`, `head_block_number: u64`, `current_payload_id: Option<PayloadId>`. Methods:
  - `async update_fork_choice(&mut self, attrs: Option<BasePayloadAttributes>) -> Result<PayloadId>`: 10s timeout, `engine_forkchoiceUpdatedV3(ForkchoiceState { head, safe, finalized: all head_block_hash }, attrs)`.
  - `async get_built_payload(&self, id: PayloadId) -> Result<ExecutionPayloadEnvelope>`: 240s timeout, `engine_getPayloadV4(id)`.
  - `async new_payload(&mut self, payload: BaseExecutionPayloadV4, beacon_root: B256) -> Result<PayloadStatus>`: 30s timeout, `engine_newPayloadV4(payload, vec![], beacon_root, vec![])`, update `head_block_hash` and `head_block_number`.
- [ ] 4.2 Implement `SequencerConsensusClient` in `src/consensus/mod.rs`: wraps `BaseConsensusClient`. `async propose(&mut self, mempool: &FakeMempool, block_time: Duration, gas_limit: u64) -> Result<(BaseExecutionPayloadV4, BlockMetrics)>`:
  1. Drain txs from mempool, chunk into batches of 100
  2. Send batches in parallel via `JoinSet` — each batch: `eth_sendRawTransaction` on node RPC
  3. `generate_payload_attributes()`:
     - `timestamp` = `head_block_timestamp + 1`
     - `prev_randao` = `B256::ZERO`
     - `suggested_fee_recipient` = `params::SUGGESTED_FEE_RECIPIENT`
     - `withdrawals` = `vec![]`
     - `parent_beacon_block_root` = `keccak256(b"fake-beacon-block-root\x01")`
     - `transactions` = vec![L1BlockInfo deposit tx with all-zero L1 fields encoded as `TxDeposit`]
     - `no_tx_pool` = false
     - `gas_limit` = gas_limit
     - `eip_1559_params` = Holocene-encoded (elasticity=50, denominator=1)
     - `min_base_fee` = 1
  4. `update_fork_choice(Some(attrs))` → payload_id
  5. `tokio::time::sleep(block_time)`
  6. `get_built_payload(payload_id)` → payload
  7. `new_payload(payload, beacon_root)` → update head
  8. Collect timing into `BlockMetrics`, return `(payload, metrics)`
- [ ] 4.3 Implement `SyncingConsensusClient` in `src/consensus/mod.rs`: `async start(&mut self, payloads: &[BaseExecutionPayloadV4], first_test_block: u64, block_time: Duration) -> Result<Vec<BlockMetrics>>`: for each payload: `new_payload()`, `update_fork_choice(None)`, sleep to cadence, collect metrics if `block_number >= first_test_block`.
- [ ] 4.4 Implement `FakeMempool` in `src/consensus/mod.rs`: `Arc<Mutex<VecDeque<Bytes>>>`. `add_transactions(txs: Vec<Bytes>)`. `drain() -> Vec<Bytes>`: take all, move deposit txs (first byte == 0x7E) to front of returned vec.
- [ ] 4.5 Tests: FakeMempool drain ordering (deposits first). Payload attributes generation (L1BlockInfo encoding, EIP-1559 params).

## Phase 5: Payload Worker [PENDING]

- [ ] 5.1 Implement JSON-RPC proxy in `src/proxy.rs`: Axum HTTP server on port from `PortManager`. `POST /` handler:
  - Parse JSON body, inspect `method` field
  - If `"eth_sendRawTransaction"`: decode `params[0]` hex → `Bytes`, push to `Arc<Mutex<Vec<Bytes>>>` buffer, AND forward to upstream via `reqwest`, return upstream response
  - All other methods: forward transparently to upstream RPC
  - `drain_pending_txs() -> Vec<Bytes>`, `url() -> String`
  - `async stop()`: axum graceful shutdown
- [ ] 5.2 Define `Worker` trait in `src/payload/mod.rs`:
  ```rust
  #[async_trait]
  pub trait Worker: Send + Sync {
      async fn setup(&mut self, rpc_url: &str, block_watcher_url: &str, flashblocks_ws_url: Option<&str>) -> Result<(), BenchmarkError>;
      fn send_txs(&self) -> Vec<Bytes>;
      async fn stop(&mut self) -> Result<(), BenchmarkError>;
      fn mempool(&self) -> &FakeMempool;
  }
  ```
- [ ] 5.3 Define `LoadTestPayloadDefinition` in `src/payload/mod.rs`: `sender_count: u64`, `funding_amount: U256`, `transactions: Vec<WeightedTx>`. Default tx mix: 70% transfer, 20% calldata 256B, 10% sha256 precompile.
- [ ] 5.4 Implement `LoadTestPayloadWorker` in `src/payload/mod.rs`:
  - `setup()`: start `RpcProxy` pointing at upstream `rpc_url`, serialize `LoadConfig` YAML to `tempfile::NamedTempFile` (rpc=proxy.url(), sender_count, target_gps=gas_limit/block_time_secs, duration="99999s", seed=rand::random::<u64>(), funding_amount, transactions, block_watcher_url, flashblocks_ws_url), spawn `load_test_bin` via `ProcessHandle` with `FUNDER_KEY=<hex_prefund_key>`.
  - `send_txs()`: `proxy.drain_pending_txs()` → `mempool.add_transactions()`, return drained.
  - `stop()`: kill process, stop proxy, drop temp file (auto-deleted by `NamedTempFile`).
- [ ] 5.5 Tests: proxy captures eth_sendRawTransaction (mock upstream). Proxy forwards other methods transparently.

## Phase 6: Metrics [PENDING]

- [ ] 6.1 Implement Prometheus scraper in `src/metrics/mod.rs`: `async fn scrape(url: &str) -> Result<Vec<prometheus_parse::Sample>, BenchmarkError>`: HTTP GET, parse with `prometheus_parse::Scrape::parse()`.
- [ ] 6.2 Implement `BlockMetrics` in `src/metrics/mod.rs`: `block_number: u64`, `timestamp: Instant`, `prev_metrics: HashMap<String, prometheus_parse::Sample>`, `execution_metrics: HashMap<String, f64>`. `update_prometheus_metric(&mut self, name: &str, current: &Sample)`: Histogram/Summary → `(sum-prev_sum)/(count-prev_count)`, skip if delta_count==0 (NaN guard); Gauge/Counter → raw value. `add_execution_metric(&mut self, name: &str, value: f64)`.
- [ ] 6.3 Define metric name consts: `SEND_TXS_LATENCY`, `UPDATE_FORK_CHOICE_LATENCY`, `GET_PAYLOAD_LATENCY`, `GAS_PER_BLOCK`, `GAS_PER_SECOND`, `TRANSACTIONS_PER_BLOCK`, `NEW_PAYLOAD_LATENCY`.
- [ ] 6.4 Implement `MetricsCollector` trait: `async fn collect(&mut self, block: &mut BlockMetrics) -> Result<(), BenchmarkError>`, `fn get_metrics(&self) -> &[BlockMetrics]`. Implement `PrometheusCollector`: scrapes node metrics port each block, calls `update_prometheus_metric()` for each known metric name.
- [ ] 6.5 Implement `FileMetricsWriter`: `write(metrics: &[BlockMetrics], path: &Path) -> Result<(), BenchmarkError>` — serialize as JSON array.
- [ ] 6.6 Implement threshold checking: `check_thresholds(metrics: &[BlockMetrics], config: &MetricsConfig) -> Vec<ThresholdViolation>`. `pub struct ThresholdViolation { metric: String, value: f64, bound: f64, severity: Severity }`. Log: `warn!(metric = %name, value = %v, bound = %b, "metric threshold exceeded")`.
- [ ] 6.7 Tests: BlockMetrics delta logic (Histogram, Gauge, NaN guard). Threshold checking (pass, warn, error).

## Phase 7: Flashblocks [PENDING]

- [ ] 7.1 Import (do NOT redefine) `FlashblocksPayloadV1` and `ExecutionPayloadFlashblockDeltaV1` from `base-common-flashblocks`.
- [ ] 7.2 Implement `FlashblocksClient` in `src/flashblocks/mod.rs`: `tokio-tungstenite` WS connect to builder's flashblocks port. Receive + deserialize `FlashblocksPayloadV1` messages. Store in `Arc<Mutex<HashMap<u64, Vec<FlashblocksPayloadV1>>>>` keyed by base block number. `get_flashblocks(block: u64) -> Vec<FlashblocksPayloadV1>`. `async stop()`: close WS.
- [ ] 7.3 Implement `FlashblockReplayServer` in `src/flashblocks/mod.rs`: Axum WebSocket server on port from `PortManager`. On client connect: replay each block's flashblocks with timed delays matching original cadence within block_time. `tokio::sync::broadcast` channel internally. `url() -> String`. `async stop()`.
- [ ] 7.4 Tests: FlashblocksClient stores by block number. ReplayServer replays within block time window.

## Phase 8: Network Benchmark Orchestration [PENDING]

- [ ] 8.1 Define `TestConfig` in `src/runner/mod.rs`: `params: BenchmarkDefinition`, `config: RollupConfig`, `genesis: Genesis`, `batcher_key: B256`, `prefund_private_key: B256`, `prefund_amount: U256`.
- [ ] 8.2 Implement `NetworkBenchmark` in `src/runner/mod.rs`: fields `sequencer_client: Box<dyn ExecutionClient>`, `validator_client: Box<dyn ExecutionClient>`, `sequencer_metrics: Vec<BlockMetrics>`, `validator_metrics: Vec<BlockMetrics>`, `test_config: TestConfig`, `transaction_payload: TransactionPayloadDef`, `port_manager: Arc<PortManager>`, `flashblocks_block_time: Option<Duration>`. `async run(&mut self) -> Result<(), BenchmarkError>`.
- [ ] 8.3 Implement `benchmark_sequencer(&mut self) -> Result<SequencerResult>`:
  1. Create `LoadTestPayloadWorker` from payload config
  2. `eth_getBlockByNumber("latest")` on sequencer → head hash + number
  3. `fund_test_account()` if balance insufficient
  4. Setup loop: spawn `worker.setup()` in `tokio::spawn`, propose blocks with `SETUP_GAS_LIMIT` until setup complete (signal via `tokio::sync::oneshot`)
  5. `first_test_block = head_block_number + 1`
  6. Benchmark loop for `num_blocks`: `worker.send_txs()`, `sequencer.propose(mempool, block_time, DEFAULT_GAS_LIMIT)`, collect `BlockMetrics`, collect flashblocks if builder
  7. `worker.stop()`
  8. Return payloads + flashblocks map + first_test_block
- [ ] 8.4 Implement `benchmark_validator(&mut self, result: SequencerResult) -> Result<()>`:
  1. Get validator head via `eth_getBlockByNumber("latest")`
  2. Catch-up: for each block from (validator_head+1) to last_setup_block: fetch from sequencer, `new_payload()` + `fcu(None)`, no metrics
  3. If flashblocks present: start `FlashblockReplayServer`, acquire port, set validator websocket URL
  4. `SyncingConsensusClient::start(payloads, first_test_block, block_time)` → collect `BlockMetrics`
- [ ] 8.5 Implement `fund_test_account(&mut self)`: `eth_getBalance(prefund_address)`, if below `PREFUND_AMOUNT`: build `TxDeposit { from: Address::from([1u8; 20]), to: TxKind::Call(prefund_address), mint: Some(PREFUND_AMOUNT), value: PREFUND_AMOUNT, gas_limit: 1_000_000, ..Default::default() }`, encode with 0x7E prefix, inject via `FakeMempool`, propose one block, retry `eth_getTransactionReceipt` until confirmed.
- [ ] 8.6 Tests: deposit tx RLP encoding. fund_test_account flow (with mock RPC).

## Phase 9: Service Layer & Main [PENDING]

- [ ] 9.1 Implement `BenchmarkRollupConfig` in `src/rollup.rs`: `pub struct BenchmarkRollupConfig { genesis: Genesis }`. `pub fn build(&self, block_time: u64) -> RollupConfig`: populate from `params.rs` consts, all OP fork times = 0 (Bedrock/Regolith/Canyon/Delta/Ecotone/Fjord/Granite/Holocene/Isthmus/Jovian).
- [ ] 9.2 Implement `setup_internal_directories(test_dir, genesis, snapshot_config) -> Result<SetupResult>` in `src/service.rs`: create `test_dir/metrics/`, generate 32-byte random JWT via `rand`, write hex to `test_dir/jwt_secret` via `base-jwt`, write genesis JSON to `test_dir/chain.json`. Call `SnapshotManager::ensure_snapshot()` if snapshot configured, else create empty datadir.
- [ ] 9.3 Implement `run_test(test_run: &TestRun, cli: &Cli) -> Result<(), BenchmarkError>` in `src/service.rs`: resolve genesis (`snapshot.genesis_file` or `build_test_genesis()`), setup sequencer + validator dirs, build `TestConfig` with `params.rs` consts, construct + run `NetworkBenchmark`, call `export_output()` in `finally`-style (via `scopeguard` or manual error handling).
- [ ] 9.4 Implement `export_output(test_dir, output_dir, role_metrics) -> Result<()>`: `FileMetricsWriter::write()`, `copy_metrics()` × 2, `gzip_file()` × 2, `write_result_json()` × 2. On error path: `dump_log_tail()`.
- [ ] 9.5 Implement `run_benchmark(cli: &Cli) -> Result<()>`: read + parse `BenchmarkConfig` YAML, call `expand()`, generate `benchmark_run_id` if not set, for each `TestRun`: `info!(run_id = %id, test = %name, index = %i, "starting test run")`, create output dir, call `run_test()`, log outcome. Continue on failure.
- [ ] 9.6 Wire `src/bin/base_bench.rs`: parse `Cli`, resolve binary paths (flag or `current_exe().parent().unwrap().join(name)`), init tracing, call `run_benchmark(&cli)`, `std::process::exit` on error.
- [ ] 9.7 Tests: `BenchmarkRollupConfig::build()` produces correct fork times. `setup_internal_directories` creates expected layout.

## Phase 10: Examples & Docs [PENDING]

- [ ] 10.1 Write `examples/devnet.yaml`: minimal benchmark config for local devnet with base-reth-node client, 100 blocks, load-test payload type, single variable dimension.
- [ ] 10.2 Write `examples/snapshot.sh`: example snapshot script with `#!/bin/bash`, `NODE_TYPE=$1`, `SNAPSHOT_PATH=$2`, shows expected interface.
- [ ] 10.3 Write `README.md`: goal, architecture diagram (sequencer → validator flow), quick start, config reference table, relationship to `base-load-tests`.

## Implementation Notes

- **2026-05-04**: `BaseEngineClient` requires `l1_rpc` URL via `EngineClientBuilder`. Use dummy URL `http://127.0.0.1:1`. L1 provider is never called — L1BlockInfo constructed manually with all-zero fields. `ref:bg_32780db9`
- **2026-05-04**: Proxy follows `ingress-rpc` crate pattern (axum + JSON parsing). Flashblocks replay follows `websocket-proxy` crate pattern (axum WS + broadcast channel). `ref:bg_7b0383d2`
- **2026-05-04**: `ProcessHandle` uses `nix::sys::signal::kill` for SIGINT (Tokio's `Child::kill()` sends SIGKILL directly). 5s grace then SIGKILL. `ref:bg_17a5e958`
- **2026-05-04**: DepositTx injection needed for mainnet snapshots. Reuse `TxDeposit` + `DEPOSIT_TX_TYPE_ID` from `base-common-consensus`. `ref:bg_32780db9`
- **2026-05-04**: Types reused from workspace (do not reimplement): `FlashblocksPayloadV1`, `ExecutionPayloadFlashblockDeltaV1` (base-common-flashblocks); `TxDeposit`, `DEPOSIT_TX_TYPE_ID` (base-common-consensus); `L1BlockInfo` (base-common-evm); `BasePayloadAttributes`, `BaseExecutionPayloadV4` (base-common-rpc-types-engine). `ref:bg_32780db9`
- **2026-05-04**: `base-reth-node` and `base-builder` binaries use macro-driven reth CLI bootstrap — cannot be imported as libraries, must be subprocess. Binary paths via `--reth-bin`/`--builder-bin` flags, default to sibling of current exe.
