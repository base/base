//! End-to-end benchmark orchestration: snapshot preparation, node lifecycle,
//! block production loop, metrics collection, and result serialization.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alloy_rpc_types_engine::JwtSecret;
use base_common_genesis::RollupConfig;
use base_test_utils::build_test_genesis;
use reqwest::Url;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::client::{setup_node, ClientOptions, InternalClientOptions};
use crate::config::{BenchmarkConfig, TestRun};
use crate::git::GitInfo;
use crate::deploy::deploy_uniswap_v3;
use crate::consensus::{
    BaseConsensusClient, FakeMempool, SequencerConsensusClient, SyncingConsensusClient,
};
use crate::error::BenchmarkError;
use crate::metrics::{
    check_thresholds, BlockMetrics, MetricsCollector, Severity, ThresholdViolation,
    GAS_PER_BLOCK, GAS_PER_SECOND, GET_PAYLOAD_LATENCY, NEW_PAYLOAD_LATENCY,
    TRANSACTIONS_PER_BLOCK,
};
use crate::output::{average_metric, write_metadata_json, write_metrics_file, RunContext};
use crate::payload::{LoadTestConfig, LoadTestPayloadWorker, PayloadWorker};
use crate::ports::PortManager;
use crate::proxy::run_proxy;
use crate::snapshots::SnapshotManager;

const JWT_SECRET: [u8; 32] = [0u8; 32];

/// Filesystem paths and keys needed by the benchmark runner.
#[derive(Debug)]
pub struct RunnerOptions {
    /// Path to the `base-reth-node` binary.
    pub reth_bin: PathBuf,
    /// Path to the `base-builder` binary.
    pub builder_bin: PathBuf,
    /// Path to the `base-load-test` binary.
    pub load_test_bin: PathBuf,
    /// Path to the benchmark YAML config file, if one was provided (recorded in metadata).
    pub config_path: Option<PathBuf>,
    /// Directory for writing results and metrics.
    pub output_dir: PathBuf,
    /// Hex-encoded private key for pre-funding test accounts.
    pub prefund_key: String,
    /// Unique identifier grouping all runs in this invocation.
    pub run_group_id: String,
    /// Git commit and branch at process startup.
    pub git_info: GitInfo,
    /// User-supplied key-value tags from `--tags`.
    pub tags: HashMap<String, String>,
}

/// Top-level orchestrator: expands the config matrix, runs each test, and
/// collects results.
#[derive(Debug)]
pub struct NetworkBenchmark {
    config: BenchmarkConfig,
    options: RunnerOptions,
    port_manager: Arc<PortManager>,
    snapshot_manager: SnapshotManager,
}

impl NetworkBenchmark {
    /// Create a new benchmark runner.
    pub fn new(config: BenchmarkConfig, options: RunnerOptions, snapshot_dir: PathBuf) -> Self {
        Self {
            config,
            options,
            port_manager: Arc::new(PortManager::new()),
            snapshot_manager: SnapshotManager::new(snapshot_dir),
        }
    }

    /// Expand the config matrix and execute every test run sequentially.
    pub async fn run_all(&mut self) -> Result<Vec<RunResult>, BenchmarkError> {
        let runs = self.config.expand()?;
        let mut results = Vec::with_capacity(runs.len());
        for run in runs {
            let result = self.run_one(run).await?;
            results.push(result);
        }
        Ok(results)
    }

    async fn run_one(&mut self, mut run: TestRun) -> Result<RunResult, BenchmarkError> {
        info!(run_id = %run.id, "starting benchmark run");

        let test_dir = tempfile::Builder::new()
            .prefix(&format!("base-bench-{}-", run.id))
            .tempdir()
            .map_err(BenchmarkError::Io)?;

        let jwt_path = test_dir.path().join("jwt.hex");
        tokio::fs::write(&jwt_path, hex::encode(JWT_SECRET))
            .await
            .map_err(BenchmarkError::Io)?;

        let data_dir = if let Some(snap_cfg) = &run.definition.snapshot {
            self.snapshot_manager
                .ensure_snapshot(
                    &run.definition.datadir,
                    snap_cfg,
                    &run.definition.node_type,
                    "sequencer",
                )
                .await?
        } else {
            run.definition
                .datadir
                .sequencer
                .clone()
                .unwrap_or_else(|| test_dir.path().join("sequencer-data"))
        };

        let flashblocks_block_time_ms = self.config.flashblocks.as_ref().map(|f| f.block_time_ms);

        let mut client_options = ClientOptions {
            node_type: run.definition.node_type.clone(),
            extra_args: vec![],
            reth_bin: self.options.reth_bin.clone(),
            builder_bin: self.options.builder_bin.clone(),
            flashblocks_block_time_ms,
        };
        if let Some(node_args) = run.definition.node_args.as_deref() {
            client_options
                .extra_args
                .extend(node_args.split_whitespace().map(ToString::to_string));
        }

        let sequencer_log_dir = self.options.output_dir.join("sequencer");
        let validator_log_dir = self.options.output_dir.join("validator");
        std::fs::create_dir_all(&sequencer_log_dir)?;
        std::fs::create_dir_all(&validator_log_dir)?;

        let chain_cfg_path = test_dir.path().join("genesis.json");
        let rollup_cfg_path = test_dir.path().join("rollup.json");
        if let Some(src) = self.config.rollup_config.as_ref() {
            tokio::fs::copy(src, &rollup_cfg_path)
                .await
                .map_err(BenchmarkError::Io)?;
            let raw = tokio::fs::read_to_string(&rollup_cfg_path)
                .await
                .map_err(BenchmarkError::Io)?;
            let rollup: RollupConfig = serde_json::from_str(&raw)
                .map_err(|e| BenchmarkError::Config(format!("invalid rollup config: {e}")))?;
            let genesis_json =
                genesis_json_from_rollup_config(&rollup, &self.options.prefund_key);
            let genesis_str = serde_json::to_string_pretty(&genesis_json)
                .map_err(|e| BenchmarkError::Config(format!("genesis json error: {e}")))?;
            tokio::fs::write(&chain_cfg_path, genesis_str)
                .await
                .map_err(BenchmarkError::Io)?;
        } else {
            let genesis = build_test_genesis();
            let genesis_str = serde_json::to_string_pretty(&genesis)
                .map_err(|e| BenchmarkError::Config(format!("genesis json error: {e}")))?;
            tokio::fs::write(&chain_cfg_path, genesis_str)
                .await
                .map_err(BenchmarkError::Io)?;
        }

        let internal_options = InternalClientOptions {
            jwt_secret_path: jwt_path,
            chain_cfg_path: chain_cfg_path.clone(),
            data_dir_path: data_dir,
            test_dir_path: sequencer_log_dir.clone(),
            jwt_secret: JWT_SECRET,
            metrics_path: test_dir.path().join("metrics"),
        };

        let mut node = setup_node(
            client_options,
            internal_options,
            Arc::clone(&self.port_manager),
            self.config.block_time_ms,
        );

        node.run().await?;
        info!(
            version = %node.get_version().await.unwrap_or_default(),
            "sequencer started"
        );

        let proxy_port = self.port_manager.acquire()?;
        let cancel = CancellationToken::new();

        let mempool = FakeMempool::new();
        let upstream: Url = node.rpc_url().parse().map_err(|_| {
            BenchmarkError::Config(format!("invalid rpc url: {}", node.rpc_url()))
        })?;

        let proxy_cancel = cancel.clone();
        let proxy_mempool = mempool.clone();
        let proxy_upstream = upstream;
        tokio::spawn(async move {
            if let Err(e) = run_proxy(proxy_port, proxy_upstream, proxy_mempool, proxy_cancel).await
            {
                warn!(error = %e, "proxy exited with error");
            }
        });

        let proxy_url: Url = format!("http://127.0.0.1:{proxy_port}")
            .parse()
            .map_err(|_| BenchmarkError::Config("invalid proxy url".into()))?;

        let auth_url: Url = node.auth_rpc_url().parse().map_err(|_| {
            BenchmarkError::Config(format!("invalid auth url: {}", node.auth_rpc_url()))
        })?;

        let rollup_cfg: Arc<RollupConfig> = if rollup_cfg_path.exists() {
            let raw = tokio::fs::read_to_string(&rollup_cfg_path)
                .await
                .map_err(BenchmarkError::Io)?;
            Arc::new(
                serde_json::from_str(&raw)
                    .map_err(|e| BenchmarkError::Config(format!("invalid rollup config: {e}")))?,
            )
        } else {
            Arc::new(RollupConfig::default())
        };

        let jwt = JwtSecret::from_hex(hex::encode(JWT_SECRET))
            .map_err(|e| BenchmarkError::Config(format!("jwt error: {e}")))?;
        let mut base = BaseConsensusClient::connect(auth_url, jwt, Arc::clone(&rollup_cfg)).await?;
        base.init_from_genesis(node.rpc_url()).await?;
        let mut sequencer = SequencerConsensusClient::new(base, node.rpc_url().to_owned());

        let mut metrics_collector = MetricsCollector::new(node.metrics_port());

        let block_time = std::time::Duration::from_millis(self.config.block_time_ms);
        let gas_limit = self.config.gas_limit.unwrap_or(30_000_000);

        let needs_uniswap_deploy = run
            .definition
            .payload
            .params
            .transactions
            .iter()
            .any(|tx| tx.tx_type == "uniswap_v3" && tx.router.is_none());

        if needs_uniswap_deploy {
            let deploy_mempool = FakeMempool::new();
            let rpc_url = node.rpc_url().to_string();
            let prefund_key = self.options.prefund_key.clone();
            let mut deploy_fut = std::pin::pin!(deploy_uniswap_v3(&rpc_url, &prefund_key));
            let addrs = loop {
                tokio::select! {
                    biased;
                    result = &mut deploy_fut => break result?,
                    result = sequencer.propose(&deploy_mempool, block_time, gas_limit) => { result?; },
                }
            };
            for tx in &mut run.definition.payload.params.transactions {
                if tx.tx_type == "uniswap_v3" {
                    if tx.router.is_none() {
                        tx.router = Some(addrs.router.to_string());
                    }
                    if tx.token_in.is_none() {
                        tx.token_in = Some(addrs.token_in.to_string());
                    }
                    if tx.token_out.is_none() {
                        tx.token_out = Some(addrs.token_out.to_string());
                    }
                }
            }
        }

        let worker = LoadTestPayloadWorker::new(LoadTestConfig {
            bin: self.options.load_test_bin.clone(),
            rpc_proxy_url: proxy_url,
            block_watcher_url: None,
            flashblocks_ws_url: None,
            params: run.definition.payload.params.clone(),
            funder_key: self.options.prefund_key.clone(),
            log_path: Some(sequencer_log_dir.join("load-test.log")),
            mempool: mempool.clone(),
        });

        worker.start().await?;

        // Setup phase: propose blocks (unmeasured) until the load test finishes
        // funding wallets and signals ready via stdout.
        let mut ready = std::pin::pin!(worker.wait_until_ready());
        loop {
            tokio::select! {
                biased;
                result = &mut ready => {
                    result?;
                    break;
                }
                result = sequencer.propose(&mempool, block_time, gas_limit) => {
                    result?;
                }
            }
        }

        let mut block_metrics_vec = Vec::with_capacity(self.config.num_blocks as usize);
        let mut payloads = Vec::with_capacity(self.config.num_blocks as usize);

        for _block_num in 0..self.config.num_blocks {
            let (payload, mut block_metrics) =
                sequencer.propose(&mempool, block_time, gas_limit).await?;
            metrics_collector.collect(&mut block_metrics).await?;
            block_metrics_vec.push(block_metrics);
            payloads.push(payload);
        }

        worker.stop().await?;
        cancel.cancel();
        node.stop().await?;

        self.port_manager.release(proxy_port);

        let validator_data_dir = test_dir.path().join("validator-data");
        std::fs::create_dir_all(&validator_data_dir)?;

        let validator_client_options = ClientOptions {
            node_type: "base-reth-node".to_string(),
            extra_args: vec![],
            reth_bin: self.options.reth_bin.clone(),
            builder_bin: self.options.builder_bin.clone(),
            flashblocks_block_time_ms: None,
        };
        let validator_internal_options = InternalClientOptions {
            jwt_secret_path: test_dir.path().join("jwt.hex"),
            chain_cfg_path: chain_cfg_path.clone(),
            data_dir_path: validator_data_dir,
            test_dir_path: validator_log_dir,
            jwt_secret: JWT_SECRET,
            metrics_path: test_dir.path().join("validator-metrics"),
        };
        let mut validator_node = setup_node(
            validator_client_options,
            validator_internal_options,
            Arc::clone(&self.port_manager),
            self.config.block_time_ms,
        );

        validator_node.run().await?;

        let validator_auth_url: Url = validator_node.auth_rpc_url().parse().map_err(|_| {
            BenchmarkError::Config(format!(
                "invalid validator auth url: {}",
                validator_node.auth_rpc_url()
            ))
        })?;
        let validator_jwt = JwtSecret::from_hex(hex::encode(JWT_SECRET))
            .map_err(|e| BenchmarkError::Config(format!("validator jwt error: {e}")))?;
        let mut validator_base =
            BaseConsensusClient::connect(validator_auth_url, validator_jwt, Arc::clone(&rollup_cfg))
                .await?;
        validator_base
            .init_from_genesis(validator_node.rpc_url())
            .await?;
        let mut validator = SyncingConsensusClient::new(validator_base);
        let mut validator_metrics_collector =
            MetricsCollector::new(validator_node.metrics_port());
        let validator_metrics = validator
            .start(&payloads, 1, block_time, &mut validator_metrics_collector)
            .await?;

        validator_node.stop().await?;

        let violations = run.definition.metrics.as_ref().map_or_else(Vec::new, |mc| {
            check_thresholds(&block_metrics_vec, mc)
        });

        let success = violations.iter().all(|v| v.severity != Severity::Error);
        write_metrics_file(&self.options.output_dir, "sequencer", &block_metrics_vec)?;
        write_metrics_file(&self.options.output_dir, "validator", &validator_metrics)?;
        let ctx = RunContext {
            run_group_id: &self.options.run_group_id,
            git_sha: &self.options.git_info.sha,
            git_branch: &self.options.git_info.branch,
            global_tags: &self.options.tags,
            success,
        };
        write_metadata_json(
            &self.options.output_dir,
            self.options.config_path.as_deref(),
            &run,
            &self.config,
            &block_metrics_vec,
            &validator_metrics,
            &ctx,
        )?;

        info!(run_id = %run.id, "run complete");

        Ok(RunResult {
            id: run.id,
            block_metrics: block_metrics_vec,
            validator_block_metrics: validator_metrics,
            violations,
        })
    }
}

/// Outcome of a single benchmark run.
#[derive(Debug)]
pub struct RunResult {
    /// Unique run identifier.
    pub id: String,
    /// Sequencer per-block metrics.
    pub block_metrics: Vec<BlockMetrics>,
    /// Validator per-block metrics.
    pub validator_block_metrics: Vec<BlockMetrics>,
    /// Threshold violations detected after the run.
    pub violations: Vec<ThresholdViolation>,
}

impl RunResult {
    /// Print a human-readable summary table to stdout.
    pub fn print_summary(&self) {
        let seq_stat = |metric: &str| -> (f64, f64) {
            let vals: Vec<f64> = self
                .block_metrics
                .iter()
                .filter_map(|b| b.execution_metrics.get(metric).copied())
                .collect();
            if vals.is_empty() {
                return (0.0, 0.0);
            }
            let avg = vals.iter().sum::<f64>() / vals.len() as f64;
            let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            (avg, max)
        };
        let val_stat = |metric: &str| -> f64 {
            let vals: Vec<f64> = self
                .validator_block_metrics
                .iter()
                .filter_map(|b| b.execution_metrics.get(metric).copied())
                .collect();
            if vals.is_empty() {
                return 0.0;
            }
            vals.iter().sum::<f64>() / vals.len() as f64
        };

        let (gas_ps_avg, gas_ps_max) = seq_stat(GAS_PER_SECOND);
        let (gas_pb_avg, gas_pb_max) = seq_stat(GAS_PER_BLOCK);
        let (txs_avg, txs_max) = seq_stat(TRANSACTIONS_PER_BLOCK);
        let gp_avg_ms = seq_stat(GET_PAYLOAD_LATENCY).0 / 1_000_000.0;
        let np_avg_ms = val_stat(NEW_PAYLOAD_LATENCY) / 1_000_000.0;

        let n_seq = self.block_metrics.len();
        let n_val = self.validator_block_metrics.len();
        let n_viol = self.violations.len();

        let gas_ps_avg_m = gas_ps_avg / 1e6;
        let gas_ps_max_m = gas_ps_max / 1e6;
        let gas_pb_avg_m = gas_pb_avg / 1e6;
        let gas_pb_max_m = gas_pb_max / 1e6;

        println!();
        println!("══════════════════════════════════════════");
        println!("  Run {id}", id = self.id);
        println!("  {n_seq:>4} seq blocks · {n_val:>4} val blocks · {n_viol:>2} violations");
        println!("──────────────────────────────────────────");
        println!("  Throughput");
        println!("    Gas/s:      {gas_ps_avg_m:>8.1} Mgas/s avg");
        println!("                {gas_ps_max_m:>8.1} Mgas/s peak");
        println!("    Gas/block:  {gas_pb_avg_m:>8.2} M avg  (peak {gas_pb_max_m:.2} M)");
        println!("    Txs/block:  {txs_avg:>8.1} avg    (max {txs_max:.0})");
        println!("──────────────────────────────────────────");
        println!("  Latency");
        println!("    get_payload: {gp_avg_ms:>9.4} ms avg");
        println!("    new_payload: {np_avg_ms:>9.4} ms avg");
        println!("══════════════════════════════════════════");
        println!();
    }
}

fn genesis_json_from_rollup_config(
    rollup: &RollupConfig,
    prefund_key: &str,
) -> serde_json::Value {
    let chain_id = rollup.l2_chain_id.id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_secs();

    let mut config = serde_json::json!({
        "chainId": chain_id,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "muirGlacierBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "mergeForkBlock": 0,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
    });

    macro_rules! set_if_some {
        ($key:expr, $val:expr) => {
            if let Some(v) = $val {
                config[$key] = serde_json::json!(v);
            }
        };
    }

    set_if_some!("regolithTime", rollup.hardforks.regolith_time);
    set_if_some!("canyonTime", rollup.hardforks.canyon_time);
    set_if_some!("deltaTime", rollup.hardforks.delta_time);
    set_if_some!("ecotoneTime", rollup.hardforks.ecotone_time);
    set_if_some!("fjordTime", rollup.hardforks.fjord_time);
    set_if_some!("graniteTime", rollup.hardforks.granite_time);
    set_if_some!("holoceneTime", rollup.hardforks.holocene_time);
    set_if_some!("isthmusTime", rollup.hardforks.isthmus_time);
    set_if_some!("jovianTime", rollup.hardforks.jovian_time);

    let alloc = {
        use alloy_signer_local::PrivateKeySigner;
        let key = prefund_key.trim_start_matches("0x");
        let signer = PrivateKeySigner::from_bytes(
            &alloy_primitives::hex::decode(key)
                .expect("valid hex prefund key")
                .as_slice()
                .try_into()
                .expect("32-byte private key"),
        )
        .expect("valid private key");
        let addr = format!("{:?}", signer.address());
        serde_json::json!({
            addr: {
                "balance": "0x3635C9ADC5DEA00000000",
            }
        })
    };

    serde_json::json!({
        "config": config,
        "difficulty": "0x0",
        "gasLimit": "0x1C9C380",
        "timestamp": format!("0x{:x}", timestamp),
        "alloc": alloc,
        "number": "0x0",
        "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_options_fields_accessible() {
        let opts = RunnerOptions {
            reth_bin: PathBuf::from("/bin/reth"),
            builder_bin: PathBuf::from("/bin/builder"),
            load_test_bin: PathBuf::from("/bin/load-test"),
            config_path: Some(PathBuf::from("/tmp/config.yaml")),
            output_dir: PathBuf::from("/tmp/bench"),
            prefund_key: "0xdef".into(),
            run_group_id: "test-group".into(),
            git_info: GitInfo { sha: "abc".into(), branch: "main".into() },
            tags: HashMap::new(),
        };
        assert_eq!(opts.reth_bin, PathBuf::from("/bin/reth"));
    }
}
