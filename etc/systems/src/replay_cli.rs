//! `base-bench replay`: measures a validator by feeding it the blocks a snapshot sequencer run
//! exported, on a datadir restored from the same snapshot.
//!
//! One execution node starts on the restored datadir with the run's chain spec, rebuilt from the
//! export manifest. Each exported block is inserted through the engine API exactly as the
//! follow client's insert task does it: `newPayload`, then a forkchoice update to the new block.
//! The command records per-block engine latency, writes `replay-result.json`, and fails when the
//! node does not end on the exported head hash and state root.

use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{
    ExecutionPayloadInputV2, ForkchoiceState, JwtSecret, PayloadStatus, PayloadStatusEnum,
};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types_engine::BaseExecutionPayload;
use base_consensus_engine::BaseEngineClient;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::ValiditySignatureMode;
use base_node_core::args::{ProofsHistoryDbBackend, RollupArgs};
use base_node_runner::BaseNodeExtension;
use base_proofs_extension::ProofsHistoryExtension;
use clap::Args;
use eyre::{Result, WrapErr, ensure};
use serde::Serialize;
use tracing::warn;

use crate::{
    ChainSpecSource, DevnetBlockInterval, ExportedBlock, InProcessClient, InProcessClientConfig,
    InProcessNodeRuntime, LocalBenchmarkArgs, PrometheusBlockCollector, ProofsSummary,
    ReplayManifest, ReplayProofsProbe, SnapshotBenchmarkArgs, SnapshotBenchmarkReportConfig,
    SnapshotChainConfig, SnapshotL2Stack, VisualizerBlockMetrics, read_export_manifest,
    read_exported_blocks,
};

/// File the replay result is written to inside `--output-dir`.
pub const REPLAY_RESULT_FILE: &str = "replay-result.json";
/// File the per-block validator Prometheus metrics are written to inside `--output-dir`.
pub const REPLAY_METRICS_FILE: &str = "metrics-validator.json";

/// Placeholder trusted peer for the replay node. [`InProcessClient`] always configures one
/// trusted peer; the replay node must not sync from anyone, so it gets an unreachable one. The
/// node ID is the secp256k1 generator point, a valid public key that no peer will answer for.
const UNREACHABLE_PEER_ENODE: &str = "enode://79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8@127.0.0.1:1";
/// Placeholder `rollup.sequencer` URL; the replay node receives no transactions to forward.
const UNUSED_SEQUENCER_URL: &str = "http://127.0.0.1:1";
/// Exported blocks decoded ahead of the engine calls, off the async runtime.
const PREFETCH_BLOCKS: usize = 64;

/// Arguments for replaying an exported snapshot run into one validator node.
#[derive(Debug, Args)]
pub struct ReplayBenchmarkArgs {
    /// Writable datadir restored from the snapshot the exporting run started from.
    #[arg(long)]
    pub datadir: PathBuf,
    /// Export directory written by `base-bench snapshot --export-blocks`.
    #[arg(long)]
    pub export_dir: PathBuf,
    /// Directory for `replay-result.json` and `metrics-validator.json`.
    #[arg(long)]
    pub output_dir: PathBuf,
    /// Send each block at its recorded time instead of as fast as the node accepts them.
    #[arg(long)]
    pub paced: bool,
    /// Minimum time between Prometheus scrapes, in milliseconds. Counter deltas are split evenly
    /// across the blocks inserted since the previous scrape.
    #[arg(long, env = "BASE_BENCH_METRICS_SCRAPE_INTERVAL_MS", default_value = "1000")]
    pub metrics_scrape_interval_ms: NonZeroU64,
    /// Maximum time to wait for graceful shutdown after writing results. Zero terminates the
    /// process immediately because the restored datadir is disposable.
    #[arg(long, default_value_t = 0)]
    pub shutdown_timeout_seconds: u64,
    /// Proof-history store matching the datadir, such as `<datadir>/proofs` from
    /// `base snapshot download --proofs`. Starts the node with proof history and records how far
    /// it trails the inserted head.
    #[arg(long)]
    pub proofs_dir: Option<PathBuf>,
    /// Proofs mode only: blocks probed at once with the proof host's L2 calls; blocks proof
    /// history covers while all are busy are skipped.
    #[arg(long, default_value = "2")]
    pub proofs_concurrency: NonZeroUsize,
}

/// How blocks were scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReplayMode {
    /// Each block waited for its recorded offset from the first block.
    Paced,
    /// Each block was sent as soon as the previous one was canonical.
    Unpaced,
}

/// Mean and percentiles of a set of durations, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencySummary {
    /// Arithmetic mean.
    pub mean_ms: f64,
    /// Nearest-rank median.
    pub p50_ms: f64,
    /// Nearest-rank 99th percentile.
    pub p99_ms: f64,
    /// Maximum.
    pub max_ms: f64,
}

impl LatencySummary {
    /// Summarizes millisecond samples; all fields are zero without samples.
    pub fn from_millis(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        let rank = |percentile: f64| {
            let index = (percentile / 100.0 * sorted.len() as f64).ceil() as usize;
            sorted[index.clamp(1, sorted.len()) - 1]
        };
        Self {
            mean_ms: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p50_ms: rank(50.0),
            p99_ms: rank(99.0),
            max_ms: sorted[sorted.len() - 1],
        }
    }
}

/// Engine timings for one replayed block.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayBlockTiming {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Block timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Gas used by the block.
    pub gas_used: u64,
    /// Transactions in the block, including system and deposit transactions.
    pub transaction_count: u64,
    /// `engine_newPayload` round trip.
    pub new_payload_ms: f64,
    /// `engine_forkchoiceUpdated` round trip.
    pub forkchoice_ms: f64,
    /// Paced mode only: how late `newPayload` was sent relative to the block's schedule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lag_ms: Option<f64>,
}

/// Machine-readable outcome of one replay, written to `replay-result.json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
    /// The export manifest the replay verified against.
    pub manifest: ReplayManifest,
    /// Paced or unpaced scheduling.
    pub mode: ReplayMode,
    /// Blocks inserted and made canonical.
    pub blocks_replayed: u64,
    /// Time from the first `newPayload` to the last forkchoice update, in milliseconds.
    pub wall_time_ms: f64,
    /// `blocks_replayed` divided by the wall time.
    pub blocks_per_second: f64,
    /// `engine_newPayload` latency.
    pub new_payload: LatencySummary,
    /// `engine_forkchoiceUpdated` latency.
    pub forkchoice: LatencySummary,
    /// Paced mode only: lag behind the recorded schedule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paced_lag: Option<LatencySummary>,
    /// Node head number after the replay.
    pub final_number: u64,
    /// Node head hash after the replay.
    pub final_hash: B256,
    /// Node head state root after the replay.
    pub final_state_root: B256,
    /// Whether the final hash and state root equal the manifest's last block.
    #[serde(rename = "match")]
    pub matches: bool,
    /// Error that stopped the replay early, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Per-block timings in replay order.
    pub blocks: Vec<ReplayBlockTiming>,
    /// Proofs mode only. Recorded without a threshold; it does not affect `match`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proofs: Option<ProofsSummary>,
}

/// Blocks inserted before the replay finished or failed.
#[derive(Debug)]
struct ReplayRun {
    blocks: Vec<ReplayBlockTiming>,
    wall_time: Duration,
    error: Option<eyre::Report>,
}

impl ReplayBenchmarkArgs {
    /// Starts the node, replays the export, writes results, and exits nonzero on a mismatch.
    pub async fn run(self) -> Result<()> {
        let manifest = read_export_manifest(&self.export_dir)?;
        let (chain_spec, block_interval) = Self::replay_chain_spec(&manifest)?;
        Self::prepare_output_dir(&self.output_dir)?;
        // Opening a directory without a store would create an empty one that covers no block.
        let proofs_store = match &self.proofs_dir {
            Some(dir) if dir.join("CURRENT").exists() => {
                Some((dir, ProofsHistoryDbBackend::Rocksdb))
            }
            Some(dir) if dir.join("mdbx.dat").exists() => Some((dir, ProofsHistoryDbBackend::Mdbx)),
            Some(dir) => eyre::bail!("{} holds no RocksDB or MDBX proofs store", dir.display()),
            None => None,
        };

        let jwt_secret = JwtSecret::random();
        let client = InProcessClient::start(InProcessClientConfig {
            runtime: InProcessNodeRuntime::Host,
            chain_spec: ChainSpecSource::Parsed(Arc::new(chain_spec)),
            datadir: Some(self.datadir.clone()),
            jwt_secret,
            builder_rpc_url: UNUSED_SEQUENCER_URL.to_string(),
            builder_flashblocks_url: None,
            builder_p2p_enode: UNREACHABLE_PEER_ENODE.to_string(),
            http_port: None,
            ws_port: None,
            auth_port: None,
            p2p_port: None,
            metrics_port: None,
            // Same persistence settings as the snapshot benchmark's follow client.
            persistence_threshold: Some(0),
            persistence_backpressure_threshold: Some(
                block_interval.snapshot_persistence_backpressure_threshold(),
            ),
            tx_forwarding_config: None,
            validity_signature_mode: ValiditySignatureMode::Off,
            upgrade_signal: None,
            extra_extensions: proofs_store
                .into_iter()
                .map(|(dir, proofs_history_db)| {
                    Box::new(ProofsHistoryExtension::new(RollupArgs {
                        proofs_history: true,
                        proofs_history_storage_path: Some(dir.clone()),
                        proofs_history_db,
                        ..Default::default()
                    })) as Box<dyn BaseNodeExtension>
                })
                .collect(),
        })
        .await
        .wrap_err("failed to start replay node")?;

        let output_result = self.execute(&client, manifest, block_interval, jwt_secret).await;
        if let Err(error) = &output_result {
            eprintln!("replay failed: {error:?}");
            let _ = std::io::stderr().flush();
        }

        if self.shutdown_timeout_seconds == 0 {
            // Results are flushed and the restored datadir is disposable: skip runtime
            // destructors that can wait on non-cancellable Reth work.
            std::process::exit(if output_result.is_ok() { 0 } else { 1 });
        }
        let shutdown_result = SnapshotBenchmarkArgs::shutdown_with_deadline(
            "replay node",
            client.shutdown(),
            self.shutdown_timeout_seconds,
        )
        .await;
        output_result?;
        shutdown_result
    }

    /// Replays every exported block, writes the artifacts, and verifies the final head.
    async fn execute(
        &self,
        client: &InProcessClient,
        manifest: ReplayManifest,
        block_interval: DevnetBlockInterval,
        jwt_secret: JwtSecret,
    ) -> Result<()> {
        let rpc_url = client.rpc_url()?;
        let provider = RootProvider::<Base>::new_http(rpc_url.clone());
        let (head_number, head_hash, _) =
            Self::block_header(&provider, BlockNumberOrTag::Latest).await?;
        ensure!(
            (head_number, head_hash) == (manifest.boundary_number, manifest.boundary_hash),
            "replay datadir head {head_number} {head_hash} is not the export boundary {} {}; \
             restore the snapshot first",
            manifest.boundary_number,
            manifest.boundary_hash
        );
        // Keep the snapshot's safe and finalized heads, which are ancestors of every replayed
        // block. A snapshot without them falls back to the boundary.
        let safe = Self::block_hash_or(&provider, BlockNumberOrTag::Safe, head_hash).await?;
        let finalized =
            Self::block_hash_or(&provider, BlockNumberOrTag::Finalized, head_hash).await?;
        let engine = BaseEngineClient::<RootProvider, RootProvider<Base>>::rpc_client::<Base>(
            client.engine_url()?,
            jwt_secret,
        )
        .await
        .wrap_err("failed to connect to the replay node's engine API")?;

        let probe = self.proofs_dir.is_some().then(|| {
            ReplayProofsProbe::start(
                rpc_url.clone(),
                self.proofs_concurrency,
                manifest.boundary_number,
            )
        });
        let metrics = PrometheusBlockCollector::start(
            rpc_url,
            client.metrics_url()?,
            Duration::from_millis(self.metrics_scrape_interval_ms.get()),
        )
        .await?;
        let run = self
            .replay_blocks(&engine, &manifest, safe, finalized, probe.as_ref().map(|p| &p.head))
            .await;
        let replayed_all = run.error.is_none();
        let proofs = match probe {
            Some(probe) => {
                probe.finish().await.inspect_err(|error| warn!(%error, "proofs probe failed")).ok()
            }
            None => None,
        };

        // After a rejected block the node may not answer; record an unknown final head (zeros,
        // so `match` is false) and keep the result rather than losing the timings and error.
        let (final_number, final_hash, final_state_root) =
            match Self::block_header(&provider, BlockNumberOrTag::Latest).await {
                Ok(head) => head,
                Err(error) if run.error.is_some() => {
                    warn!(%error, "could not read the node head after a rejected block");
                    (0, B256::ZERO, B256::ZERO)
                }
                Err(error) => return Err(error),
            };
        let result = ReplayResult {
            proofs,
            ..Self::summarize(
                manifest,
                if self.paced { ReplayMode::Paced } else { ReplayMode::Unpaced },
                run,
                (final_number, final_hash, final_state_root),
            )
        };
        let result_path = self.output_dir.join(REPLAY_RESULT_FILE);
        LocalBenchmarkArgs::write_json(&result_path, &result, "replay result")?;
        println!("replay result: {}", result_path.display());
        std::io::stdout().flush().wrap_err("failed to flush replay result output")?;
        if replayed_all {
            let metrics = metrics.finish(result.manifest.last_number).await?;
            let samples = Self::visualizer_metrics(&result.blocks, &metrics, block_interval)?;
            LocalBenchmarkArgs::write_json(
                &self.output_dir.join(REPLAY_METRICS_FILE),
                &samples,
                "replay validator metrics",
            )?;
        }

        if let Some(error) = &result.error {
            eyre::bail!("replay stopped after {} blocks: {error}", result.blocks_replayed);
        }
        ensure!(
            result.matches,
            "replay ended at {final_number} {final_hash} (state root {final_state_root}), expected \
             {} {} (state root {})",
            result.manifest.last_number,
            result.manifest.last_hash,
            result.manifest.last_state_root
        );
        Ok(())
    }

    /// Inserts the exported blocks in order, stopping at the first failure.
    async fn replay_blocks(
        &self,
        engine: &RootProvider<Base>,
        manifest: &ReplayManifest,
        safe: B256,
        finalized: B256,
        head: Option<&tokio::sync::watch::Sender<u64>>,
    ) -> ReplayRun {
        let mut blocks = Vec::with_capacity(manifest.block_count as usize);
        let mut first_timestamp_ms = None;
        // Read and decode on a blocking thread so JSON decoding neither stalls the runtime the
        // node shares nor counts toward replay time; the clock starts at the first decoded block.
        let (decoded_tx, mut decoded_rx) = tokio::sync::mpsc::channel(PREFETCH_BLOCKS);
        let export_dir = self.export_dir.clone();
        let reader = tokio::task::spawn_blocking(move || match read_exported_blocks(&export_dir) {
            Ok(exported) => {
                for block in exported {
                    if decoded_tx.blocking_send(block).is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = decoded_tx.blocking_send(Err(error));
            }
        });
        let mut replay_start = None;
        let error = async {
            let mut index = 0u64;
            while let Some(block) = decoded_rx.recv().await {
                let replay_start = *replay_start.get_or_insert_with(Instant::now);
                let block = block?;
                let expected_number = manifest.boundary_number + 1 + index;
                index += 1;
                ensure!(
                    block.number == expected_number,
                    "exported block {} is out of order; expected {expected_number}",
                    block.number
                );
                let first = *first_timestamp_ms.get_or_insert(block.timestamp_ms);
                let lag = if self.paced {
                    let target = replay_start + Self::schedule_offset(first, block.timestamp_ms)?;
                    tokio::time::sleep_until(target.into()).await;
                    Some(Instant::now().saturating_duration_since(target))
                } else {
                    None
                };
                blocks.push(Self::insert(engine, &block, safe, finalized, lag).await?);
                if let Some(head) = head {
                    head.send_replace(block.number);
                }
            }
            ensure!(
                blocks.len() as u64 == manifest.block_count,
                "export holds {} blocks but the manifest records {}",
                blocks.len(),
                manifest.block_count
            );
            Ok(())
        }
        .await
        .err();
        let wall_time = replay_start.map(|start| start.elapsed()).unwrap_or_default();
        // Closing the channel stops the reader if the replay ended early.
        drop(decoded_rx);
        let _ = reader.await;
        ReplayRun { blocks, wall_time, error }
    }

    /// Inserts one block with `newPayload` and makes it canonical with a forkchoice update.
    async fn insert(
        engine: &RootProvider<Base>,
        block: &ExportedBlock,
        safe: B256,
        finalized: B256,
        lag: Option<Duration>,
    ) -> Result<ReplayBlockTiming> {
        let payload = &block.envelope.execution_payload;
        let version = match payload {
            BaseExecutionPayload::V1(_) => 1,
            BaseExecutionPayload::V2(_) => 2,
            BaseExecutionPayload::V3(_) => 3,
            BaseExecutionPayload::V4(_) => 4,
        };
        ensure!(
            version == block.payload_version,
            "block {} records payload version {} but carries a V{version} payload",
            block.number,
            block.payload_version
        );
        let parent_beacon_block_root = block.envelope.parent_beacon_block_root.unwrap_or_default();
        let gas_used = payload.as_v1().gas_used;
        let transaction_count = payload.as_v1().transactions.len() as u64;

        // Copy the payload before starting the timer so `newPayloadMs` measures only the node.
        let request = payload.clone();
        let started = Instant::now();
        let status = match request {
            BaseExecutionPayload::V1(payload) => {
                engine
                    .new_payload_v2(ExecutionPayloadInputV2 {
                        execution_payload: payload,
                        withdrawals: None,
                    })
                    .await
            }
            BaseExecutionPayload::V2(payload) => {
                engine
                    .new_payload_v2(ExecutionPayloadInputV2 {
                        execution_payload: payload.payload_inner,
                        withdrawals: Some(payload.withdrawals),
                    })
                    .await
            }
            BaseExecutionPayload::V3(payload) => {
                engine.new_payload_v3(payload, parent_beacon_block_root).await
            }
            BaseExecutionPayload::V4(payload) => {
                engine.new_payload_v4(payload, parent_beacon_block_root).await
            }
        }
        .wrap_err_with(|| format!("newPayload failed for block {}", block.number))?;
        let new_payload = started.elapsed();
        Self::ensure_valid(&status, "newPayload", block)?;

        let started = Instant::now();
        let updated = engine
            .fork_choice_updated_v3(
                ForkchoiceState {
                    head_block_hash: block.hash,
                    safe_block_hash: safe,
                    finalized_block_hash: finalized,
                },
                None,
            )
            .await
            .wrap_err_with(|| format!("forkchoiceUpdated failed for block {}", block.number))?;
        let forkchoice = started.elapsed();
        Self::ensure_valid(&updated.payload_status, "forkchoiceUpdated", block)?;

        Ok(ReplayBlockTiming {
            number: block.number,
            hash: block.hash,
            timestamp_ms: block.timestamp_ms,
            gas_used,
            transaction_count,
            new_payload_ms: Self::millis(new_payload),
            forkchoice_ms: Self::millis(forkchoice),
            lag_ms: lag.map(Self::millis),
        })
    }

    fn ensure_valid(status: &PayloadStatus, call: &str, block: &ExportedBlock) -> Result<()> {
        ensure!(
            matches!(status.status, PayloadStatusEnum::Valid),
            "{call} returned {:?} for block {} {}",
            status.status,
            block.number,
            block.hash
        );
        Ok(())
    }

    /// Builds the result document from the replayed blocks and the node's final head.
    fn summarize(
        manifest: ReplayManifest,
        mode: ReplayMode,
        run: ReplayRun,
        (final_number, final_hash, final_state_root): (u64, B256, B256),
    ) -> ReplayResult {
        let collect = |field: fn(&ReplayBlockTiming) -> Option<f64>| {
            run.blocks.iter().filter_map(field).collect::<Vec<_>>()
        };
        let wall_seconds = run.wall_time.as_secs_f64();
        let blocks_replayed = run.blocks.len() as u64;
        ReplayResult {
            mode,
            blocks_replayed,
            wall_time_ms: Self::millis(run.wall_time),
            blocks_per_second: if wall_seconds > 0.0 {
                blocks_replayed as f64 / wall_seconds
            } else {
                0.0
            },
            new_payload: LatencySummary::from_millis(&collect(|block| Some(block.new_payload_ms))),
            forkchoice: LatencySummary::from_millis(&collect(|block| Some(block.forkchoice_ms))),
            paced_lag: (mode == ReplayMode::Paced)
                .then(|| LatencySummary::from_millis(&collect(|block| block.lag_ms))),
            final_number,
            final_hash,
            final_state_root,
            matches: final_hash == manifest.last_hash
                && final_state_root == manifest.last_state_root,
            error: run.error.map(|error| format!("{error:#}")),
            blocks: run.blocks,
            proofs: None,
            manifest,
        }
    }

    /// Joins per-block Prometheus samples with block totals in the visualizer format used by the
    /// snapshot benchmark's `metrics-validator.json`.
    fn visualizer_metrics(
        blocks: &[ReplayBlockTiming],
        metrics: &BTreeMap<u64, BTreeMap<String, f64>>,
        block_interval: DevnetBlockInterval,
    ) -> Result<Vec<VisualizerBlockMetrics>> {
        let interval_ms = block_interval.duration().as_millis() as u64;
        let block_seconds = interval_ms as f64 / 1_000.0;
        blocks
            .iter()
            .enumerate()
            .map(|(index, block)| {
                let scraped = metrics.get(&block.number).cloned().ok_or_else(|| {
                    eyre::eyre!("Prometheus sample for replayed block {} is missing", block.number)
                })?;
                Ok(VisualizerBlockMetrics {
                    block_number: SnapshotBenchmarkReportConfig::normalized_block_number(
                        index,
                        interval_ms,
                    ),
                    timestamp: block.timestamp_ms,
                    execution_metrics: SnapshotBenchmarkReportConfig::block_visualizer_metrics(
                        scraped,
                        block.gas_used,
                        block.transaction_count,
                        block_seconds,
                    ),
                })
            })
            .collect()
    }

    /// Resolves the chain the way `base-bench snapshot` does and applies the run's schedule:
    /// Denim at the first produced block for 200 ms, never for 2 s.
    pub fn replay_chain_spec(
        manifest: &ReplayManifest,
    ) -> Result<(BaseChainSpec, DevnetBlockInterval)> {
        let block_interval = match manifest.block_interval_ms {
            2_000 => DevnetBlockInterval::TwoSeconds,
            200 => DevnetBlockInterval::TwoHundredMilliseconds,
            other => {
                eyre::bail!("unsupported export block interval {other}ms; expected 2000 or 200")
            }
        };
        let chain = SnapshotChainConfig {
            chain: manifest.chain.clone(),
            rollup_config: manifest.rollup_config.clone(),
        }
        .resolve()?;
        let chain_spec = SnapshotL2Stack::chain_spec(
            (*chain.chain_spec).clone(),
            manifest.first_block_timestamp,
            block_interval,
        );
        Ok((chain_spec, block_interval))
    }

    /// Offset of a block from the first exported block, which paced replay waits for.
    pub fn schedule_offset(first_timestamp_ms: u64, timestamp_ms: u64) -> Result<Duration> {
        let offset = timestamp_ms.checked_sub(first_timestamp_ms).ok_or_else(|| {
            eyre::eyre!(
                "block timestamp {timestamp_ms}ms precedes first block {first_timestamp_ms}ms"
            )
        })?;
        Ok(Duration::from_millis(offset))
    }

    async fn block_header(
        provider: &RootProvider<Base>,
        tag: BlockNumberOrTag,
    ) -> Result<(u64, B256, B256)> {
        let block = provider
            .get_block_by_number(tag)
            .await?
            .ok_or_else(|| eyre::eyre!("replay node has no {tag} block"))?;
        Ok((block.header.number, block.header.hash, block.header.state_root))
    }

    async fn block_hash_or(
        provider: &RootProvider<Base>,
        tag: BlockNumberOrTag,
        fallback: B256,
    ) -> Result<B256> {
        Ok(provider.get_block_by_number(tag).await?.map_or(fallback, |block| block.header.hash))
    }

    /// Creates the output directory and removes stale replay artifacts.
    fn prepare_output_dir(path: &Path) -> Result<()> {
        fs::create_dir_all(path).wrap_err_with(|| {
            format!("failed to create replay output directory {}", path.display())
        })?;
        for file in [REPLAY_RESULT_FILE, REPLAY_METRICS_FILE] {
            let stale = path.join(file);
            if stale.exists() {
                fs::remove_file(&stale)
                    .wrap_err_with(|| format!("failed to remove stale {}", stale.display()))?;
            }
        }
        Ok(())
    }

    fn millis(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1_000.0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::B256;
    use base_common_chains::Upgrades;
    use base_common_genesis::BaseUpgrade;
    use clap::Parser;
    use reth_ethereum_forks::ForkCondition;

    use super::{LatencySummary, ReplayBenchmarkArgs, ReplayBlockTiming, ReplayMode, ReplayRun};
    use crate::{BenchmarkCli, BenchmarkCommand, DevnetBlockInterval, ReplayManifest};

    const FIRST_BLOCK_TIMESTAMP: u64 = 2_000_000_000;

    fn manifest(block_interval_ms: u64) -> ReplayManifest {
        ReplayManifest {
            chain: "mainnet".to_string(),
            rollup_config: None,
            block_interval_ms,
            boundary_number: 30_000_000,
            boundary_hash: B256::repeat_byte(1),
            first_block_timestamp: FIRST_BLOCK_TIMESTAMP,
            last_number: 30_000_002,
            last_hash: B256::repeat_byte(2),
            last_state_root: B256::repeat_byte(3),
            block_count: 2,
        }
    }

    fn timing(number: u64, new_payload_ms: f64, lag_ms: Option<f64>) -> ReplayBlockTiming {
        ReplayBlockTiming {
            number,
            hash: B256::with_last_byte(number as u8),
            timestamp_ms: FIRST_BLOCK_TIMESTAMP * 1_000,
            gas_used: 21_000,
            transaction_count: 2,
            new_payload_ms,
            forkchoice_ms: 1.0,
            lag_ms,
        }
    }

    #[test]
    fn parses_replay_defaults_to_unpaced() {
        let cli = BenchmarkCli::parse_from([
            "base-bench",
            "replay",
            "--datadir",
            "/snapshot/restored",
            "--export-dir",
            "/boot/export",
            "--output-dir",
            "results/replay",
        ]);

        let Some(BenchmarkCommand::Replay(args)) = cli.command else {
            panic!("expected replay command");
        };
        assert_eq!(args.datadir.to_string_lossy(), "/snapshot/restored");
        assert_eq!(args.export_dir.to_string_lossy(), "/boot/export");
        assert!(!args.paced);
        assert_eq!(args.shutdown_timeout_seconds, 0);
        assert!(args.proofs_dir.is_none());
    }

    #[test]
    fn parses_proofs_mode() {
        let cli = BenchmarkCli::parse_from([
            "base-bench",
            "replay",
            "--datadir",
            "/snapshot/restored",
            "--export-dir",
            "/boot/export",
            "--output-dir",
            "results/replay",
            "--proofs-dir",
            "/snapshot/restored/proofs",
        ]);

        let Some(BenchmarkCommand::Replay(args)) = cli.command else {
            panic!("expected replay command");
        };
        assert_eq!(args.proofs_dir.unwrap().to_string_lossy(), "/snapshot/restored/proofs");
        assert_eq!(args.proofs_concurrency.get(), 2);
    }

    #[test]
    fn paced_schedule_offsets_follow_block_timestamps() {
        let first = FIRST_BLOCK_TIMESTAMP * 1_000;

        assert_eq!(ReplayBenchmarkArgs::schedule_offset(first, first).unwrap(), Duration::ZERO);
        assert_eq!(
            ReplayBenchmarkArgs::schedule_offset(first, first + 4 * 200).unwrap(),
            Duration::from_millis(800)
        );
        assert_eq!(
            ReplayBenchmarkArgs::schedule_offset(first, first + 2_000).unwrap(),
            Duration::from_secs(2)
        );
        assert!(ReplayBenchmarkArgs::schedule_offset(first, first - 200).is_err());
    }

    #[test]
    fn latency_summary_uses_nearest_rank_percentiles() {
        let samples = (1..=100).rev().map(f64::from).collect::<Vec<_>>();

        let summary = LatencySummary::from_millis(&samples);

        assert_eq!(
            summary,
            LatencySummary { mean_ms: 50.5, p50_ms: 50.0, p99_ms: 99.0, max_ms: 100.0 }
        );
        assert_eq!(
            LatencySummary::from_millis(&[7.0]),
            LatencySummary { mean_ms: 7.0, p50_ms: 7.0, p99_ms: 7.0, max_ms: 7.0 }
        );
        assert_eq!(LatencySummary::from_millis(&[]), LatencySummary::default());
    }

    #[test]
    fn summary_reports_rate_lag_and_match() {
        let manifest = manifest(200);
        let run = || ReplayRun {
            blocks: vec![timing(30_000_001, 4.0, Some(0.0)), timing(30_000_002, 8.0, Some(10.0))],
            wall_time: Duration::from_millis(500),
            error: None,
        };
        let last = (manifest.last_number, manifest.last_hash, manifest.last_state_root);

        let result =
            ReplayBenchmarkArgs::summarize(manifest.clone(), ReplayMode::Paced, run(), last);

        assert!(result.matches);
        assert_eq!(result.blocks_replayed, 2);
        assert_eq!(result.blocks_per_second, 4.0);
        assert_eq!(result.new_payload.mean_ms, 6.0);
        assert_eq!(result.new_payload.max_ms, 8.0);
        assert_eq!(result.paced_lag.unwrap().max_ms, 10.0);

        let unpaced =
            ReplayBenchmarkArgs::summarize(manifest.clone(), ReplayMode::Unpaced, run(), last);
        assert!(unpaced.paced_lag.is_none());

        let wrong_root = (last.0, last.1, B256::repeat_byte(9));
        let mismatch =
            ReplayBenchmarkArgs::summarize(manifest, ReplayMode::Unpaced, run(), wrong_root);
        assert!(!mismatch.matches);
    }

    #[test]
    fn subsecond_manifest_activates_denim_at_first_block() {
        let (chain_spec, interval) =
            ReplayBenchmarkArgs::replay_chain_spec(&manifest(200)).unwrap();

        assert_eq!(interval, DevnetBlockInterval::TwoHundredMilliseconds);
        assert_eq!(
            chain_spec.fork(BaseUpgrade::Denim),
            ForkCondition::Timestamp(FIRST_BLOCK_TIMESTAMP)
        );
        assert!(!chain_spec.is_denim_active_at_timestamp(FIRST_BLOCK_TIMESTAMP - 1));
        assert!(chain_spec.is_denim_active_at_timestamp(FIRST_BLOCK_TIMESTAMP));
    }

    #[test]
    fn two_second_manifest_never_activates_denim() {
        let (chain_spec, interval) =
            ReplayBenchmarkArgs::replay_chain_spec(&manifest(2_000)).unwrap();

        assert_eq!(interval, DevnetBlockInterval::TwoSeconds);
        assert_eq!(chain_spec.fork(BaseUpgrade::Denim), ForkCondition::Never);
    }

    #[test]
    fn rejects_unknown_block_interval() {
        let error = ReplayBenchmarkArgs::replay_chain_spec(&manifest(1_000)).unwrap_err();

        assert!(error.to_string().contains("unsupported export block interval"));
    }
}
