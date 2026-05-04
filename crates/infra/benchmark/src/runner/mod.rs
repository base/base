//! End-to-end benchmark orchestration: snapshot preparation, node lifecycle,
//! block production loop, metrics collection, and result serialization.

use std::path::PathBuf;
use std::sync::Arc;

use alloy_rpc_types_engine::JwtSecret;
use base_common_genesis::RollupConfig;
use reqwest::Url;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::client::{setup_node, ClientOptions, InternalClientOptions};
use crate::config::{BenchmarkConfig, TestRun};
use crate::consensus::{
    BaseConsensusClient, FakeMempool, SequencerConsensusClient, SyncingConsensusClient,
};
use crate::error::BenchmarkError;
use crate::metrics::{
    check_thresholds, BlockMetrics, MetricsCollector, Severity, ThresholdViolation,
};
use crate::output::{write_metadata_json, write_metrics_file};
use crate::payload::{LoadTestPayloadWorker, PayloadWorker};
use crate::ports::PortManager;
use crate::proxy::run_proxy;
use crate::snapshots::SnapshotManager;

const JWT_SECRET: [u8; 32] = [0u8; 32];

pub struct RunnerOptions {
    pub reth_bin: PathBuf,
    pub builder_bin: PathBuf,
    pub load_test_bin: PathBuf,
    pub config_path: PathBuf,
    pub output_dir: PathBuf,
    pub prefund_key: String,
}

pub struct NetworkBenchmark {
    config: BenchmarkConfig,
    options: RunnerOptions,
    port_manager: Arc<PortManager>,
    snapshot_manager: SnapshotManager,
}

impl NetworkBenchmark {
    pub fn new(config: BenchmarkConfig, options: RunnerOptions, snapshot_dir: PathBuf) -> Self {
        Self {
            config,
            options,
            port_manager: Arc::new(PortManager::new()),
            snapshot_manager: SnapshotManager::new(snapshot_dir),
        }
    }

    pub async fn run_all(&mut self) -> Result<Vec<RunResult>, BenchmarkError> {
        let runs = self.config.expand()?;
        let mut results = Vec::with_capacity(runs.len());
        for run in runs {
            let result = self.run_one(run).await?;
            results.push(result);
        }
        Ok(results)
    }

    async fn run_one(&mut self, run: TestRun) -> Result<RunResult, BenchmarkError> {
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
            let genesis_json = genesis_json_from_rollup_config(&rollup);
            let genesis_str = serde_json::to_string_pretty(&genesis_json)
                .map_err(|e| BenchmarkError::Config(format!("genesis json error: {e}")))?;
            tokio::fs::write(&chain_cfg_path, genesis_str)
                .await
                .map_err(BenchmarkError::Io)?;
        }

        let internal_options = InternalClientOptions {
            jwt_secret_path: jwt_path,
            chain_cfg_path: chain_cfg_path.clone(),
            data_dir_path: data_dir,
            test_dir_path: sequencer_log_dir,
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

        let worker = LoadTestPayloadWorker::new(
            self.options.load_test_bin.clone(),
            proxy_url,
            None,
            None,
            run.payload.params.clone(),
            self.options.prefund_key.clone(),
            mempool.clone(),
        );

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

        worker.start().await?;

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

        let violations = if let Some(mc) = &run.definition.metrics {
            check_thresholds(&block_metrics_vec, mc)
        } else {
            vec![]
        };

        let success = violations.iter().all(|v| v.severity != Severity::Error);
        write_metrics_file(&self.options.output_dir, "sequencer", &block_metrics_vec)?;
        write_metrics_file(&self.options.output_dir, "validator", &validator_metrics)?;
        write_metadata_json(
            &self.options.output_dir,
            &self.options.config_path,
            &run,
            &self.config,
            &block_metrics_vec,
            &validator_metrics,
            success,
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

pub struct RunResult {
    pub id: String,
    pub block_metrics: Vec<BlockMetrics>,
    pub validator_block_metrics: Vec<BlockMetrics>,
    pub violations: Vec<ThresholdViolation>,
}

fn genesis_json_from_rollup_config(rollup: &RollupConfig) -> serde_json::Value {
    let chain_id = rollup.l2_chain_id.id();
    let timestamp = rollup.genesis.l2_time;

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

    serde_json::json!({
        "config": config,
        "difficulty": "0x0",
        "gasLimit": "0x1C9C380",
        "timestamp": format!("0x{:x}", timestamp),
        "alloc": {},
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
            config_path: PathBuf::from("/tmp/config.yaml"),
            output_dir: PathBuf::from("/tmp/bench"),
            prefund_key: "0xdef".into(),
        };
        assert_eq!(opts.reth_bin, PathBuf::from("/bin/reth"));
    }
}
