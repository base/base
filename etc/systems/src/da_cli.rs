//! `base-bench da`: the L1 data a block range needs, measured by running the range through the
//! batcher encoder with the Rust batcher's default settings. Rates use block timestamps, so they
//! describe the range's own pace, not how fast this command runs.

use std::{collections::VecDeque, fs, path::PathBuf};

use alloy_eips::{eip2718::Encodable2718, eip7685::EMPTY_REQUESTS_HASH};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{CancunPayloadFields, PraguePayloadFields};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, BlobPayload, DaEgress, EncoderConfig, StepResult,
    SubmissionPayload,
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
use serde::Serialize;
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
#[derive(Debug, Serialize)]
pub struct DaResult {
    /// First measured block.
    pub first_block: u64,
    /// Last measured block.
    pub last_block: u64,
    /// Block time the range covers, one block interval per block.
    pub seconds: f64,
    /// Gas used by the range.
    pub gas_used: u64,
    /// Transactions in the range, deposits included.
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
    /// Encodes the range and writes `da-result.json`.
    pub async fn run(self) -> Result<()> {
        let result = if let Some(dir) = &self.export_dir {
            let manifest = read_export_manifest(dir)?;
            let mut meter = DaMeter::new(SnapshotChainConfig {
                chain: manifest.chain,
                rollup_config: manifest.rollup_config,
            })?;
            for block in read_exported_blocks(dir)? {
                meter.add_exported(block?)?;
            }
            meter.finish()?
        } else {
            let (Some(url), Some(from), Some(to)) = (&self.rpc_url, self.from, self.to) else {
                eyre::bail!("--rpc-url, --from and --to are required without --export-dir");
            };
            let mut meter = DaMeter::new(SnapshotChainConfig {
                chain: self.chain.clone(),
                rollup_config: None,
            })?;
            let provider = RootProvider::<Base>::new_http(url.clone());
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
                meter.add_exported(ExportedBlock::from_rpc_block(block?))?;
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
        let encoder = BatchEncoder::new(chain.resolve()?.rollup_config, config)?;
        Ok(Self {
            encoder,
            channel_duration,
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
        let BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload } =
            block.envelope;
        // The batch must link each block to its parent's real hash, which covers header fields
        // the payload leaves to the sidecar.
        let cancun = CancunPayloadFields::new(parent_beacon_block_root.unwrap_or_default(), vec![]);
        let block = match execution_payload {
            BaseExecutionPayload::V4(_) => {
                execution_payload.try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v4(
                    cancun,
                    PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
                ))
            }
            BaseExecutionPayload::V3(_) => execution_payload
                .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v3(cancun)),
            _ => execution_payload.try_into_block(),
        }
        .wrap_err_with(|| format!("block {number} is not a valid payload"))?;
        self.add(block, timestamp_ms)
    }

    /// Adds the next block of the range, produced at `timestamp_ms`.
    fn add(&mut self, block: BaseBlock, timestamp_ms: u64) -> Result<()> {
        let number = block.header.number;
        let (_, first_ms) = *self.first.get_or_insert((number, timestamp_ms));
        self.gas_used += block.header.gas_used;
        self.transactions += block.body.transactions.len() as u64;
        self.transaction_bytes +=
            block.body.transactions.iter().map(|tx| tx.encode_2718_len() as u64).sum::<u64>();
        // Channels close after a number of L1 blocks, so L1 time follows the range's own clock.
        self.l1_head = (timestamp_ms - first_ms) / L1_SLOT_MS;
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
        if let Some(info) = safe {
            self.encoder.reconcile_derivation(info, self.l1_head);
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
    fn finish(mut self) -> Result<DaResult> {
        self.encoder.flush()?;
        self.drain()?;
        let (first_block, first_ms) = self.first.ok_or_else(|| eyre::eyre!("no blocks"))?;
        let (last_block, last_ms) = self.last;
        let blocks = last_block - first_block + 1;
        ensure!(blocks > 1, "a range needs at least two blocks to have a block interval");
        let seconds = (last_ms - first_ms) as f64 / 1_000.0 * blocks as f64 / (blocks - 1) as f64;
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
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};

    use super::DaMeter;
    use crate::SnapshotChainConfig;

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
}
