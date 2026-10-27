//! `base-bench da`: the L1 data a block range needs, measured by running the range through the
//! batcher encoder with the Rust batcher's default settings. Rates use block timestamps, so they
//! describe the range's own pace, not how fast this command runs.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
};

use alloy_eips::{eip2718::Encodable2718, eip7685::EMPTY_REQUESTS_HASH};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{CancunPayloadFields, PraguePayloadFields};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, BlobPayload, DaEgress, DerivationReconciliation, EncoderConfig,
    StepResult, SubmissionPayload,
};
use base_common_consensus::BaseBlock;
use base_common_network::Base;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
};
use base_protocol::BlockInfo;
use clap::Args;
use eyre::{Result, WrapErr, ensure};
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{ExportedBlock, SnapshotChainConfig, read_export_manifest, read_exported_blocks};

/// File the DA result is written to inside `--output-dir`.
pub const DA_RESULT_FILE: &str = "da-result.json";

/// Ethereum's blob target per L1 block since BPO2 (2026-01-07).
const BLOB_TARGET: f64 = 14.0;
const L1_SLOT_MS: u64 = 12_000;
const RPC_FETCH_CONCURRENCY: usize = 16;

/// Arguments for measuring the DA bytes of an exported run or an RPC block range.
#[derive(Debug, Args)]
pub struct DaBenchmarkArgs {
    /// Export directory written by `base-bench snapshot --export-blocks`.
    #[arg(long, required_unless_present = "rpc_url", conflicts_with = "rpc_url")]
    pub export_dir: Option<PathBuf>,
    /// L2 RPC to read blocks `--from` to `--to` from, instead of an export.
    #[arg(long, requires_all = ["from", "to"])]
    pub rpc_url: Option<url::Url>,
    /// First block of the RPC range.
    #[arg(long)]
    pub from: Option<u64>,
    /// Last block of the RPC range.
    #[arg(long)]
    pub to: Option<u64>,
    /// Built-in chain name of the RPC range. An export names its own chain.
    #[arg(long, default_value = "mainnet")]
    pub chain: String,
    /// Directory for `da-result.json`.
    #[arg(long)]
    pub output_dir: PathBuf,
}

/// DA cost of a block range.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaResult {
    /// First measured block.
    pub first_block: u64,
    /// Last measured block.
    pub last_block: u64,
    /// Block time the range covers, one block interval per block.
    pub seconds: f64,
    /// Gas used by the range.
    pub gas_used: u64,
    /// Non-deposit transactions in the range, the ones batches carry.
    pub transactions: u64,
    /// EIP-2718 bytes of those transactions.
    pub transaction_bytes: u64,
    /// L1 batch transactions the encoder produced.
    pub l1_transactions: u64,
    /// Blobs those transactions carry.
    pub blobs: u64,
    /// Encoded frame bytes packed into the blobs, comparable to a blob's capacity.
    pub frame_bytes: u64,
    /// Gas used per second of block time.
    pub gas_per_second: f64,
    /// Frame bytes per second of block time.
    pub frame_bytes_per_second: f64,
    /// Frame bytes per unit of gas.
    pub bytes_per_gas: f64,
    /// Transaction bytes per frame byte.
    pub compression_ratio: f64,
    /// Blobs per 12 s L1 slot.
    pub blobs_per_l1_slot: f64,
    /// Used share of the produced blobs' capacity.
    pub blob_fill: f64,
    /// Gas per second that full blobs at the L1 blob target carry at this range's bytes per gas.
    pub blob_target_gas_per_second: f64,
}

impl DaBenchmarkArgs {
    /// Models a complete export range with the same batcher settings as the DA command.
    /// An explicit range is inclusive; `None` measures the complete export.
    pub fn measure_export(dir: &Path, range: Option<(u64, u64)>) -> Result<DaResult> {
        let manifest = read_export_manifest(dir)?;
        let (first, last) = range.unwrap_or((manifest.boundary_number + 1, manifest.last_number));
        ensure!(
            first > manifest.boundary_number && first <= last && last <= manifest.last_number,
            "DA range {first}..={last} is outside the export"
        );
        let mut meter = DaMeter::new(SnapshotChainConfig {
            chain: manifest.chain,
            rollup_config: manifest.rollup_config,
        })?;
        let mut count = 0;
        for block in read_exported_blocks(dir)? {
            let block = block?;
            if block.number < first {
                continue;
            }
            if block.number > last {
                break;
            }
            ensure!(block.number == first + count, "DA export range is not contiguous");
            meter.add_exported(block)?;
            count += 1;
        }
        ensure!(count == last - first + 1, "DA export range is incomplete");
        meter.finish_with_interval(Some(manifest.block_interval_ms))
    }

    /// Computes the export's DA model off the async runtime, then persists it before cleanup.
    pub async fn write_export_model(
        dir: PathBuf,
        range: Option<(u64, u64)>,
        output_dir: &Path,
    ) -> Result<DaResult> {
        let result = tokio::task::spawn_blocking(move || Self::measure_export(&dir, range))
            .await
            .wrap_err("DA model task failed")??;
        crate::LocalBenchmarkArgs::write_json(
            &output_dir.join(DA_RESULT_FILE),
            &result,
            "DA model result",
        )?;
        Ok(result)
    }

    /// Encodes the range and writes `da-result.json`.
    pub async fn run(self) -> Result<()> {
        let result = if let Some(dir) = &self.export_dir {
            Self::measure_export(dir, None)?
        } else {
            let (Some(url), Some(from), Some(to)) = (&self.rpc_url, self.from, self.to) else {
                eyre::bail!("--rpc-url, --from and --to are required without --export-dir");
            };
            let mut meter = DaMeter::new(SnapshotChainConfig {
                chain: self.chain.clone(),
                rollup_config: None,
            })?;
            let provider = RootProvider::<Base>::new_http(url.clone());
            let chain =
                SnapshotChainConfig { chain: self.chain.clone(), rollup_config: None }.resolve()?;
            let source = base_consensus_node::RemoteL2Client::new(url.clone(), chain.rollup_config);
            let mut blocks = stream::iter(from..=to)
                .map(|number| {
                    let provider = &provider;
                    async move {
                        provider
                            .get_block_by_number(BlockNumberOrTag::Number(number))
                            .full()
                            .await
                            .wrap_err_with(|| format!("failed to fetch block {number}"))?
                            .ok_or_else(|| eyre::eyre!("block {number} is unavailable"))
                    }
                })
                .buffered(RPC_FETCH_CONCURRENCY);
            while let Some(block) = blocks.next().await {
                let block = block?;
                let bal = if let Some(expected) = block.header.block_access_list_hash {
                    Some(source.get_block_access_list(block.header.hash, expected).await?)
                } else {
                    None
                };
                meter.add_exported(ExportedBlock::from_rpc_block_with_extras(block, bal))?;
            }
            meter.finish()?
        };
        fs::create_dir_all(&self.output_dir)
            .wrap_err_with(|| format!("failed to create {}", self.output_dir.display()))?;
        fs::write(self.output_dir.join(DA_RESULT_FILE), serde_json::to_vec_pretty(&result)?)
            .wrap_err("failed to write the DA result")?;
        info!(?result, "measured DA bytes");
        Ok(())
    }
}

/// Feeds blocks to a [`BatchEncoder`] and totals what it submits.
#[derive(Debug)]
struct DaMeter {
    encoder: BatchEncoder,
    channel_duration: u64,
    channel_timeout: u64,
    l1_head: u64,
    unreleased: VecDeque<(u64, BlockInfo)>,
    first: Option<(u64, u64)>,
    last: (u64, u64),
    gas_used: u64,
    transactions: u64,
    transaction_bytes: u64,
    l1_transactions: u64,
    blobs: u64,
    frame_bytes: u64,
}

impl DaMeter {
    /// Creates a meter with the Rust batcher's default encoder settings.
    fn new(chain: SnapshotChainConfig) -> Result<Self> {
        let config = EncoderConfig::default();
        let channel_duration = config.max_channel_duration;
        let rollup_config = chain.resolve()?.rollup_config;
        let channel_timeout = EncoderConfig::confirmation_channel_timeout(&rollup_config);
        let encoder = BatchEncoder::new(rollup_config, config)?;
        Ok(Self {
            encoder,
            channel_duration,
            channel_timeout,
            l1_head: 0,
            unreleased: VecDeque::new(),
            first: None,
            last: (0, 0),
            gas_used: 0,
            transactions: 0,
            transaction_bytes: 0,
            l1_transactions: 0,
            blobs: 0,
            frame_bytes: 0,
        })
    }

    /// Adds an exported block.
    fn add_exported(&mut self, block: ExportedBlock) -> Result<()> {
        let number = block.number;
        let timestamp_ms = block.timestamp_ms;
        let expected_hash = block.hash;
        let BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload } =
            block.envelope;
        // The batch must link each block to its parent's real hash, which covers header fields
        // the payload leaves to the sidecar.
        let cancun = CancunPayloadFields::new(parent_beacon_block_root.unwrap_or_default(), vec![]);
        let block = match execution_payload {
            BaseExecutionPayload::V4(_) | BaseExecutionPayload::V5(_) => execution_payload
                .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v4(
                    cancun,
                    PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
                )),
            BaseExecutionPayload::V3(_) => execution_payload
                .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v3(cancun)),
            _ => execution_payload.try_into_block(),
        }
        .wrap_err_with(|| format!("block {number} is not a valid payload"))?;
        ensure!(block.header.hash_slow() == expected_hash, "DA payload header hash mismatch");
        self.add(block, timestamp_ms)
    }

    /// Adds the next block of the range, produced at `timestamp_ms`.
    fn add(&mut self, block: BaseBlock, timestamp_ms: u64) -> Result<()> {
        let number = block.header.number;
        let (_, first_ms) = *self.first.get_or_insert((number, timestamp_ms));
        self.gas_used += block.header.gas_used;
        // Batches leave out deposits, which derivation rebuilds from L1.
        for tx in block.body.transactions.iter().filter(|tx| !tx.is_deposit()) {
            self.transactions += 1;
            self.transaction_bytes += tx.encode_2718_len() as u64;
        }
        // Channels close after a number of L1 blocks, so L1 time follows the range's own clock.
        let slot = (timestamp_ms - first_ms) / L1_SLOT_MS;
        // L1 passing a submitted channel's timeout makes the encoder resubmit it, counting it twice.
        ensure!(
            slot - self.l1_head < self.channel_timeout,
            "block {number} is {} L1 slots after the previous block, at or past the {}-slot channel timeout",
            slot - self.l1_head,
            self.channel_timeout
        );
        self.l1_head = slot;
        self.encoder.advance_l1_head(self.l1_head);
        let info = BlockInfo::new(
            block.header.hash_slow(),
            number,
            block.header.parent_hash,
            block.header.timestamp,
        );
        self.encoder
            .add_block(block)
            .map_err(|(err, _)| eyre::eyre!("block {number} does not extend the range: {err}"))?;
        self.last = (number, timestamp_ms);
        self.unreleased.push_back((self.l1_head, info));
        self.drain()?;
        // A block added a full channel duration ago sits in a closed channel whose submissions
        // are confirmed, so the encoder can drop it; otherwise it keeps the whole range.
        let mut safe = None;
        while let Some(&(slot, info)) = self.unreleased.front()
            && slot + self.channel_duration <= self.l1_head
        {
            safe = Some(info);
            self.unreleased.pop_front();
        }
        // A stalled channel is expected here: its tail is confirmed before it is released.
        if let Some(info) = safe {
            ensure!(
                !matches!(
                    self.encoder.reconcile_derivation(info, self.l1_head),
                    DerivationReconciliation::SafeHeadMismatch
                ),
                "the encoder could not release block {}",
                info.number
            );
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        while self.encoder.step()? != StepResult::Idle {}
        while let Some(submission) = self.encoder.next_submission() {
            // Submissions count as included at once; unconfirmed or stale ones are resubmitted.
            self.encoder.confirm(submission.id, self.l1_head);
            self.l1_transactions += 1;
            if let SubmissionPayload::Blobs(blobs) = submission.payload() {
                self.blobs += blobs.len() as u64;
                self.frame_bytes +=
                    blobs.iter().map(BlobPayload::frame_bytes).sum::<usize>() as u64;
            }
        }
        Ok(())
    }

    /// Flushes the open channel and computes the result.
    fn finish(self) -> Result<DaResult> {
        self.finish_with_interval(None)
    }

    /// Uses an export's cadence, including for a one-block measured window.
    fn finish_with_interval(mut self, interval_ms: Option<u64>) -> Result<DaResult> {
        self.encoder.flush()?;
        self.drain()?;
        let (first_block, first_ms) = self.first.ok_or_else(|| eyre::eyre!("no blocks"))?;
        let (last_block, last_ms) = self.last;
        let blocks = last_block - first_block + 1;
        let seconds = if let Some(interval_ms) = interval_ms {
            ensure!(interval_ms > 0, "DA block interval must be positive");
            interval_ms as f64 / 1_000.0 * blocks as f64
        } else {
            ensure!(blocks > 1, "a range needs at least two blocks to have a block interval");
            (last_ms - first_ms) as f64 / 1_000.0 * blocks as f64 / (blocks - 1) as f64
        };
        let bytes_per_gas = self.frame_bytes as f64 / self.gas_used as f64;
        let capacity = DaEgress::BLOB_CAPACITY as f64;
        Ok(DaResult {
            first_block,
            last_block,
            seconds,
            gas_used: self.gas_used,
            transactions: self.transactions,
            transaction_bytes: self.transaction_bytes,
            l1_transactions: self.l1_transactions,
            blobs: self.blobs,
            frame_bytes: self.frame_bytes,
            gas_per_second: self.gas_used as f64 / seconds,
            frame_bytes_per_second: self.frame_bytes as f64 / seconds,
            bytes_per_gas,
            compression_ratio: self.transaction_bytes as f64 / self.frame_bytes as f64,
            blobs_per_l1_slot: self.blobs as f64 / seconds * (L1_SLOT_MS / 1_000) as f64,
            blob_fill: self.frame_bytes as f64 / (self.blobs as f64 * capacity),
            blob_target_gas_per_second: BLOB_TARGET * capacity
                / (L1_SLOT_MS / 1_000) as f64
                / bytes_per_gas,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{B256, Bytes, Sealed, Signature, keccak256};
    use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};
    use clap::Parser;

    use super::{DA_RESULT_FILE, DaBenchmarkArgs, DaMeter};
    use crate::{
        BenchmarkCli, BenchmarkCommand, BlockExportWriter, ExportedBlock, ReplayManifest,
        SnapshotChainConfig,
    };

    const TIMESTAMP: u64 = 2_000_000_000;

    fn block(parent_hash: B256, number: u64) -> BaseBlock {
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
        let deposit = TxDeposit { input: calldata, ..Default::default() };
        // Incompressible input, so the frame bytes track the transaction bytes.
        let input: Bytes =
            (0..320u64).flat_map(|i| keccak256((number << 32 | i).to_be_bytes()).0).collect();
        let user_tx =
            TxLegacy { input, ..Default::default() }.into_signed(Signature::test_signature());
        BaseBlock {
            header: Header {
                parent_hash,
                number,
                timestamp: TIMESTAMP + number * 12,
                gas_used: 1_000_000,
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![
                    BaseTxEnvelope::Deposit(Sealed::new(deposit)),
                    BaseTxEnvelope::Legacy(user_tx),
                ],
                ..Default::default()
            },
        }
    }

    /// A V5 export must preserve the real parent hashes and 200ms rates through the DA model.
    #[tokio::test]
    async fn default_snapshot_and_replay_da_model_accepts_v5_exports() {
        let export = tempfile::tempdir().expect("export directory");
        let output = tempfile::tempdir().expect("result directory");
        let mut writer = BlockExportWriter::create(export.path()).expect("export writer");
        let mut parent = B256::ZERO;
        let mut transaction_bytes = 0;
        for number in 1..=3 {
            let mut block = block(parent, number);
            block.header.timestamp = TIMESTAMP;
            block.header.base_fee_per_gas = Some(0);
            block.header.parent_beacon_block_root = Some(B256::ZERO);
            block.header.withdrawals_root = Some(alloy_consensus::constants::EMPTY_WITHDRAWALS);
            block.header.requests_hash = Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH);
            block.header.blob_gas_used = Some(0);
            block.header.excess_blob_gas = Some(0);
            block.header.block_access_list_hash = Some(keccak256([0xc0]));
            block.header.slot_number = Some(number);
            block.body.withdrawals = Some(Default::default());
            block.header.transactions_root =
                alloy_consensus::proofs::calculate_transaction_root(&block.body.transactions);
            parent = block.header.hash_slow();
            if number > 1 {
                transaction_bytes +=
                    alloy_eips::Encodable2718::encode_2718_len(&block.body.transactions[1]) as u64;
            }
            let (payload, _) = BaseExecutionPayload::from_block_unchecked_with_extras(
                parent,
                &block,
                Some(Bytes::from_static(&[0xc0])),
            );
            assert!(matches!(payload, BaseExecutionPayload::V5(_)), "fixture must exercise V5");
            writer
                .write(&ExportedBlock {
                    number,
                    hash: parent,
                    timestamp_ms: TIMESTAMP * 1000 + (number - 1) * 200,
                    state_root: block.header.state_root,
                    payload_version: 5,
                    envelope: BaseExecutionPayloadEnvelope {
                        parent_beacon_block_root: Some(B256::ZERO),
                        execution_payload: payload,
                    },
                })
                .expect("write V5 block");
        }
        writer
            .finish(ReplayManifest {
                chain: "mainnet".into(),
                rollup_config: None,
                block_interval_ms: 200,
                boundary_number: 0,
                boundary_hash: B256::ZERO,
                first_block_timestamp: TIMESTAMP,
                last_number: 0,
                last_hash: B256::ZERO,
                last_state_root: B256::ZERO,
                block_count: 0,
                measurement_start_block: Some(1),
                measurement_end_block: Some(3),
            })
            .expect("finish fixture export");
        for command in ["snapshot", "replay"] {
            let arguments = if command == "snapshot" {
                vec![
                    "base-bench",
                    command,
                    "--builder-datadir",
                    "/unused",
                    "--load-test-config",
                    "load.yaml",
                    "--scenario",
                    "da",
                    "--output-dir",
                    output.path().to_str().unwrap(),
                ]
            } else {
                vec![
                    "base-bench",
                    command,
                    "--datadir",
                    "/unused",
                    "--export-dir",
                    export.path().to_str().unwrap(),
                    "--output-dir",
                    output.path().to_str().unwrap(),
                ]
            };
            let cli = BenchmarkCli::parse_from(&arguments);
            let skip = match cli.command.expect("explicit command") {
                BenchmarkCommand::Snapshot(args) => args.skip_da,
                BenchmarkCommand::Replay(args) => args.skip_da,
                _ => panic!("snapshot or replay"),
            };
            assert!(!skip, "{command} must model DA with no enabling flag");
            let mut opted_out = arguments;
            opted_out.push("--skip-da");
            let cli = BenchmarkCli::parse_from(opted_out);
            let skip = match cli.command.expect("explicit opt-out command") {
                BenchmarkCommand::Snapshot(args) => args.skip_da,
                BenchmarkCommand::Replay(args) => args.skip_da,
                _ => panic!("snapshot or replay"),
            };
            assert!(skip, "{command} must accept an explicit DA opt-out");
            let result = DaBenchmarkArgs::write_export_model(
                export.path().into(),
                Some((2, 3)),
                output.path(),
            )
            .await
            .expect("default modeled DA output");
            assert_eq!((result.first_block, result.last_block), (2, 3), "exclude setup block");
            assert_eq!(result.seconds, 0.4, "use manifest 200ms cadence, not rounded seconds");
            assert_eq!(result.gas_used, 2_000_000, "exact measured block gas");
            assert_eq!(result.transactions, 2, "exclude L1-info deposits");
            assert_eq!(result.transaction_bytes, transaction_bytes, "exact EIP-2718 bytes");
            assert_eq!(result.gas_per_second, 5_000_000.0, "exact rate over measured blocks");
            assert_eq!(result.blobs, 1, "small fixture flushes to one modeled blob");
            assert_eq!(result.l1_transactions, 1, "one modeled L1 submission");
            assert!(result.frame_bytes > 0, "the real batch encoder must emit frames");
            let json: serde_json::Value = serde_json::from_slice(
                &std::fs::read(output.path().join(DA_RESULT_FILE)).expect("automatic DA artifact"),
            )
            .expect("DA JSON");
            assert_eq!(json["transactions"], 2, "artifact must contain modeled results");
            assert_eq!(json["seconds"], 0.4, "artifact must preserve 200ms range time");
        }
        let one = DaBenchmarkArgs::measure_export(export.path(), Some((3, 3))).expect("one block");
        assert_eq!(one.seconds, 0.2, "one-block windows use the declared export cadence");
        assert_eq!(one.transactions, 1, "one-block DA model excludes deposits");
        assert!(
            DaBenchmarkArgs::measure_export(export.path(), Some((2, 4))).is_err(),
            "reject incomplete ranges"
        );
    }

    /// Blocks 12 s apart for longer than the channel timeout are each encoded once, and the rates
    /// use block time.
    #[test]
    fn measures_a_range_over_block_time() {
        let mut meter = DaMeter::new(SnapshotChainConfig::default()).unwrap();
        let mut parent_hash = B256::ZERO;
        for number in 1..=80 {
            let block = block(parent_hash, number);
            parent_hash = block.header.hash_slow();
            meter.add(block, (TIMESTAMP + number * 12) * 1_000).unwrap();
        }
        let result = meter.finish().unwrap();

        assert_eq!(result.seconds, 960.0);
        assert_eq!(result.gas_per_second, 80_000_000.0 / 960.0);
        let ratio = result.frame_bytes as f64 / result.transaction_bytes as f64;
        assert!((0.9..1.1).contains(&ratio), "frame bytes per transaction byte {ratio}");
    }

    /// A gap as long as the channel timeout would double count resubmitted channels, so it fails.
    #[test]
    fn rejects_a_gap_past_the_channel_timeout() {
        let mut meter = DaMeter::new(SnapshotChainConfig::default()).unwrap();
        let first = block(B256::ZERO, 1);
        let second = block(first.header.hash_slow(), 2);
        meter.add(first, TIMESTAMP * 1_000).unwrap();
        let err = meter.add(second, (TIMESTAMP + 60 * 12) * 1_000).unwrap_err();
        assert!(err.to_string().contains("channel timeout"), "{err}");
    }
}
