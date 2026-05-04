//! End-to-end benchmark orchestration: snapshot preparation, node lifecycle,
//! block production loop, metrics collection, and result serialization.

use std::path::PathBuf;
use std::sync::Arc;

use alloy_rpc_types_engine::JwtSecret;
use reqwest::Url;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::client::{setup_node, ClientOptions, InternalClientOptions};
use crate::config::{BenchmarkConfig, TestRun};
use crate::consensus::{BaseConsensusClient, FakeMempool, SequencerConsensusClient};
use crate::error::BenchmarkError;
use crate::metrics::{
    check_thresholds, write_metrics_json, BlockMetrics, MetricsCollector, Severity,
    ThresholdViolation,
};
use crate::output::write_result_json;
use crate::payload::{LoadTestPayloadWorker, PayloadWorker};
use crate::ports::PortManager;
use crate::proxy::run_proxy;
use crate::snapshots::SnapshotManager;

const JWT_SECRET: [u8; 32] = [0u8; 32];

pub struct RunnerOptions {
    pub reth_bin: PathBuf,
    pub builder_bin: PathBuf,
    pub load_test_bin: PathBuf,
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

        let client_options = ClientOptions {
            node_type: run.definition.node_type.clone(),
            extra_args: vec![],
            reth_bin: self.options.reth_bin.clone(),
            builder_bin: self.options.builder_bin.clone(),
            flashblocks_block_time_ms,
        };

        let metrics_port = self.port_manager.acquire()?;
        let chain_cfg_path = test_dir.path().join("rollup.json");

        let internal_options = InternalClientOptions {
            jwt_secret_path: jwt_path,
            chain_cfg_path: chain_cfg_path.clone(),
            data_dir_path: data_dir,
            test_dir_path: test_dir.path().to_path_buf(),
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

        let jwt = JwtSecret::from_hex(hex::encode(JWT_SECRET))
            .map_err(|e| BenchmarkError::Config(format!("jwt error: {e}")))?;
        let base = BaseConsensusClient::connect(auth_url, jwt, Default::default()).await?;
        let mut sequencer = SequencerConsensusClient::new(base, node.rpc_url().to_owned());

        let mut metrics_collector = MetricsCollector::new(metrics_port);

        let block_time = std::time::Duration::from_millis(self.config.block_time_ms);
        let gas_limit = 30_000_000u64;

        worker.start().await?;

        let mut block_metrics_vec = Vec::with_capacity(self.config.num_blocks as usize);

        for _block_num in 0..self.config.num_blocks {
            let (_payload, mut block_metrics) =
                sequencer.propose(&mempool, block_time, gas_limit).await?;
            metrics_collector.collect(&mut block_metrics).await?;
            block_metrics_vec.push(block_metrics);
        }

        worker.stop().await?;
        cancel.cancel();
        node.stop().await?;

        self.port_manager.release(proxy_port);
        self.port_manager.release(metrics_port);

        let violations = if let Some(mc) = &run.definition.metrics {
            check_thresholds(&block_metrics_vec, mc)
        } else {
            vec![]
        };

        let success = violations.iter().all(|v| v.severity != Severity::Error);
        let error_msg = if success { None } else { Some("threshold violations") };
        write_result_json(&self.options.output_dir, "sequencer", &run.id, success, error_msg)?;
        info!(run_id = %run.id, "run complete");

        Ok(RunResult {
            id: run.id,
            block_metrics: block_metrics_vec,
            violations,
        })
    }
}

pub struct RunResult {
    pub id: String,
    pub block_metrics: Vec<BlockMetrics>,
    pub violations: Vec<ThresholdViolation>,
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
            output_dir: PathBuf::from("/tmp/bench"),
            prefund_key: "0xdef".into(),
        };
        assert_eq!(opts.reth_bin, PathBuf::from("/bin/reth"));
    }
}


