---
status: complete
phase: 10
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
├── PLAN.md
├── examples/
│   └── devnet.yaml
└── src/
    ├── lib.rs              # Minimal: doc = include_str!("../README.md"), grouped mod + pub use
    ├── error.rs            # BenchmarkError enum (thiserror)
    ├── params.rs           # Const rollup/chain params (BATCHER_KEY, PREFUND_KEY, etc.)
    ├── config/
    │   └── mod.rs          # BenchmarkConfig, BenchmarkDefinition, matrix expansion
    ├── ports.rs            # PortManager (TcpListener probe, acquire/release)
    ├── process.rs          # ProcessHandle (SIGINT + 5s grace + SIGKILL)
    ├── snapshots.rs        # SnapshotManager (sha256[:12] cache, ensure_snapshot)
    ├── output.rs           # gzip_file, write_result_json, dump_log_tail, random_id
    ├── client/
    │   └── mod.rs          # ExecutionClient trait, BaseRethNodeClient, BuilderClient, setup_node
    ├── consensus/
    │   └── mod.rs          # BaseConsensusClient, SequencerConsensusClient, SyncingConsensusClient, FakeMempool
    ├── payload/
    │   └── mod.rs          # PayloadWorker trait, LoadTestPayloadWorker
    ├── proxy/
    │   └── mod.rs          # run_proxy (axum JSON-RPC intercept proxy)
    ├── metrics/
    │   └── mod.rs          # BlockMetrics, MetricsCollector, check_thresholds, write_metrics_json
    ├── flashblocks/
    │   └── mod.rs          # FlashblocksClient (tokio-tungstenite WS), FlashblockReplayServer
    ├── runner/
    │   └── mod.rs          # NetworkBenchmark, RunnerOptions, RunResult
    ├── service.rs          # BenchmarkArgs, run_benchmark (top-level entry point)
    └── bin/
        └── base_bench.rs   # CLI glue only (Cli struct, resolve_bin, tokio::main)
```

> **AGENTS.md rules applied:** Each `mod foo; pub use foo::*;` grouped together in lib.rs. No `pub mod` except test utils. All types `pub`. No logic in lib.rs. Binary contains only glue. `mod.rs` files begin with `//!` doc comment. Structured tracing with key=value fields.

## Phase 1: Crate Scaffold, Config & Error Types [COMPLETE]

- [x] 1.1 Create `Cargo.toml`: workspace deps (`clap`, `tokio`, `serde`, `serde_yaml`, `serde_json`, `tracing`, `tracing-subscriber`, `rand`, `thiserror`, `eyre`, `alloy`, `reqwest`, `axum`, `tokio-tungstenite`, `flate2`, `sha2`, `nix`, `prometheus-parse`, `base-consensus-engine`, `base-jwt`, `base-common-genesis`, `base-common-flashblocks`, `base-common-consensus`, `base-common-evm`, `base-test-utils`). `[[bin]] name = "base-bench" path = "src/bin/base_bench.rs"`. `[lints] workspace = true`.
- [x] 1.2 Create `src/error.rs`: `BenchmarkError` enum with variants: `Config(String)`, `Io(#[from] std::io::Error)`, `Client(String)`, `EngineApi(String)`, `Metrics(String)`, `Proxy(String)`, `Snapshot(String)`, `Timeout(String)`, `ProcessCrash { binary: String, exit_code: Option<i32> }`. All Display via thiserror.
- [x] 1.3 Create minimal `src/lib.rs` and stub `src/bin/base_bench.rs`
- [x] 1.4 Add `"crates/infra/benchmark"` to workspace `Cargo.toml` members
- [x] 1.5 Define config types in `src/config/mod.rs`
- [x] 1.6 Create `src/params.rs`: rollup consts including `BATCHER_KEY`, `PREFUND_KEY`, `DEFAULT_GAS_LIMIT`, etc.
- [x] 1.7 Implement matrix expansion in `src/config/mod.rs`: cartesian product of `variables`, ≤100 guard.
- [x] 1.8 Implement CLI in `src/bin/base_bench.rs` with all flags + `BASE_BENCH_*` env prefix.
- [x] 1.9 Tests: config deserialization round-trip, matrix expansion (0 vars, 1 var, 2+ vars, >100 error), params const sanity checks.

## Phase 2: Infrastructure Utilities [COMPLETE]

- [x] 2.1 `PortManager` in `src/ports.rs` — acquire/release/acquire_n, TcpListener::bind probe
- [x] 2.2 `ProcessHandle` in `src/process.rs` — SIGINT + 5s grace + SIGKILL, structured tracing
- [x] 2.3 `SnapshotManager` in `src/snapshots.rs` — sha256(command)[:12] cache, force_clean, explicit path passthrough
- [x] 2.4 Output helpers in `src/output.rs` — gzip, write_result_json, dump_log_tail, random_id (shipped in Phase 1)
- [x] 2.5 Tests — 20 tests total passing (ports ×5, snapshots ×6, output ×2, config ×4, matrix ×3)

## Phase 3: Client Abstraction [COMPLETE]

- [x] 3.1 `ExecutionClient` trait, `ClientOptions`, `InternalClientOptions` in `src/client/mod.rs`
- [x] 3.2 `BaseRethNodeClient` — 4 ports, full arg list, txpool backup cleanup, 240s RPC wait, SIGINT stop
- [x] 3.3 `BuilderClient` — wraps inner, acquires flashblocks WS port, appends flashblocks flags
- [x] 3.4 `setup_node()` dispatch fn — `"builder"` → `BuilderClient`, all others → `BaseRethNodeClient`
- [x] 3.5 Tests — arg list completeness, websocket URL injection, builder default block time, dispatch. `FlashblocksClient` stub added to `src/flashblocks/mod.rs` (full impl in Phase 7)
- 25 tests passing

## Phase 4: Consensus Clients [COMPLETE]

- [x] 4.1 Implement `BaseConsensusClient` in `src/consensus/mod.rs`.
- [x] 4.2 Implement `SequencerConsensusClient::propose()`: FCU→sleep→getPayload→newPayload, Holocene EIP-1559 params, fake beacon root, L1BlockInfo deposit tx.
- [x] 4.3 Implement `SyncingConsensusClient::start()`.
- [x] 4.4 Implement `FakeMempool`: deposits-first drain ordering.
- [x] 4.5 Tests: FakeMempool drain ordering, deposit tx encoding, EIP-1559 param encoding, beacon root determinism.

## Phase 5: Payload Worker [COMPLETE]

- [x] 5.1 Implement JSON-RPC proxy in `src/proxy.rs`: axum `POST /`, intercepts `eth_sendRawTransaction` into `FakeMempool`, forwards all other methods to upstream via reqwest.
- [x] 5.2 Define `PayloadWorker` trait (`start`, `stop`) in `src/payload/mod.rs`.
- [x] 5.3 Implement `LoadTestPayloadWorker`: writes temp YAML config, spawns `base-load-test` subprocess with `FUNDER_KEY` env, shares `FakeMempool` with proxy.
- [x] 5.4 Tests: drain empty, drain clears pending, proxy serialization.

## Phase 6: Metrics [COMPLETE]

- [x] 6.1 Implement `scrape_prometheus(url: &str)` via `prometheus_parse::Scrape::parse()`.
- [x] 6.2 Implement `BlockMetrics`: Histogram delta (sum/count), Gauge raw, NaN guard.
- [x] 6.3 Define metric name consts: `GAS_PER_BLOCK`, `GAS_PER_SECOND`, `TRANSACTIONS_PER_BLOCK`, `SEND_TXS_LATENCY`, `UPDATE_FORK_CHOICE_LATENCY`, `GET_PAYLOAD_LATENCY`, `NEW_PAYLOAD_LATENCY`.
- [x] 6.4 Implement `MetricsCollector`: scrapes node metrics port each block.
- [x] 6.5 Implement `write_metrics_json`: serialize `BlockMetrics` slice as JSON.
- [x] 6.6 Implement `check_thresholds(metrics, config) -> Vec<ThresholdViolation>` with `Severity::Warning/Error`.
- [x] 6.7 Tests: BlockMetrics delta logic (Histogram via real prometheus text parse, Gauge). Threshold checking (min/max violation, pass).

## Phase 7: Flashblocks [COMPLETE]

- [x] 7.1 Reuse `Flashblock::try_decode_message()` from `base-common-flashblocks` (handles JSON + Brotli).
- [x] 7.2 Implement `FlashblocksClient`: tokio-tungstenite WS consumer, exponential backoff reconnect (500ms–5s), ping/pong, `drain() -> Vec<Flashblock>`.
- [x] 7.3 Implement `FlashblockReplayServer`: axum WS upgrade + `broadcast::channel`, `broadcast_all(&[Flashblock])`, `run(port, cancel)`.
- [x] 7.4 Tests: client drain starts empty, replay broadcast to no receivers does not panic.

## Phase 8: Network Benchmark Orchestration [COMPLETE]

- [x] 8.1 Implement `NetworkBenchmark { config, options, port_manager, snapshot_manager }` with `run_all(&mut self) -> Result<Vec<RunResult>>`.
- [x] 8.2 Implement `run_one`: tempdir setup, JWT write, snapshot preparation via `SnapshotManager::ensure_snapshot`, node start, proxy spawn, `LoadTestPayloadWorker` start, `SequencerConsensusClient` block loop, `MetricsCollector::collect` per block, threshold check, result JSON write.
- [x] 8.3 `RunnerOptions { reth_bin, builder_bin, load_test_bin, output_dir, prefund_key }`.
- [x] 8.4 `RunResult { id, block_metrics, violations }`.
- [x] 8.5 Test: RunnerOptions fields accessible.

## Phase 9: Service Layer & Main [COMPLETE]

- [x] 9.1 Implement `BenchmarkArgs { config_path, output_dir, reth_bin, builder_bin, load_test_bin, prefund_key, snapshot_dir }` in `src/service.rs`.
- [x] 9.2 Implement `run_benchmark(args: BenchmarkArgs) -> Result<()>`: parse YAML config, construct `NetworkBenchmark`, run all, log per-run outcome, fail if any run has error-severity violations.
- [x] 9.3 Wire `src/bin/base_bench.rs`: reads `BASE_BENCH_PREFUND_KEY` env, constructs `BenchmarkArgs`, calls `run_benchmark`, exits non-zero on error.
- [x] 9.4 Test: `BenchmarkArgs` fields accessible.

## Phase 10: Examples & Docs [COMPLETE]

- [x] 10.1 Write `examples/devnet.yaml`: minimal benchmark config for local devnet with reth node, 20 blocks, erc20-transfer payload, warning/error metric thresholds.
- [x] 10.2 Write `README.md`: architecture diagram, usage, config field reference, matrix expansion, output description.

## Implementation Notes

- **2026-05-04**: `BaseEngineClient` requires `l1_rpc` URL via `EngineClientBuilder`. Use dummy URL `http://127.0.0.1:1`. L1 provider is never called — L1BlockInfo constructed manually with all-zero fields. `ref:bg_32780db9`
- **2026-05-04**: Proxy follows `ingress-rpc` crate pattern (axum + JSON parsing). Flashblocks replay follows `websocket-proxy` crate pattern (axum WS + broadcast channel). `ref:bg_7b0383d2`
- **2026-05-04**: `ProcessHandle` uses `nix::sys::signal::kill` for SIGINT (Tokio's `Child::kill()` sends SIGKILL directly). 5s grace then SIGKILL. `ref:bg_17a5e958`
- **2026-05-04**: DepositTx injection needed for mainnet snapshots. Reuse `TxDeposit` + `DEPOSIT_TX_TYPE_ID` from `base-common-consensus`. `ref:bg_32780db9`
- **2026-05-04**: Types reused from workspace (do not reimplement): `FlashblocksPayloadV1`, `ExecutionPayloadFlashblockDeltaV1` (base-common-flashblocks); `TxDeposit`, `DEPOSIT_TX_TYPE_ID` (base-common-consensus); `L1BlockInfo` (base-common-evm); `BasePayloadAttributes`, `BaseExecutionPayloadV4` (base-common-rpc-types-engine). `ref:bg_32780db9`
- **2026-05-04**: `base-reth-node` and `base-builder` binaries use macro-driven reth CLI bootstrap — cannot be imported as libraries, must be subprocess. Binary paths via `--reth-bin`/`--builder-bin` flags, default to sibling of current exe.
