//! Export of the blocks a snapshot sequencer run produced, for replay into a validator.
//!
//! A run writes every block after the snapshot boundary to `<dir>/blocks.jsonl`, one JSON line
//! per block in order, and then `<dir>/replay.json`. The manifest is written last, so its presence
//! marks a complete export. A replay restores the same snapshot and feeds the blocks back through
//! the engine API on a chain rebuilt from the manifest.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use alloy_consensus::{Block, BlockBody};
use alloy_primitives::B256;
use base_common_consensus::BaseTxEnvelope;
use base_common_network::Base;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};

/// File holding the exported blocks, one JSON line each.
pub const EXPORT_BLOCKS_FILE: &str = "blocks.jsonl";
/// File holding the [`ReplayManifest`]; written last.
pub const EXPORT_MANIFEST_FILE: &str = "replay.json";

/// What a replay needs besides the blocks: how to rebuild the run's chain and what to verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayManifest {
    /// `base-bench snapshot --chain` value of the run.
    pub chain: String,
    /// `base-bench snapshot --rollup-config` value of the run, for custom chains.
    pub rollup_config: Option<PathBuf>,
    /// Local block interval of the run in milliseconds (2000 or 200).
    pub block_interval_ms: u64,
    /// Snapshot head the run continued from.
    pub boundary_number: u64,
    /// Hash of the snapshot head.
    pub boundary_hash: B256,
    /// Timestamp in seconds of the first produced block, where the run anchored its schedule and,
    /// at 200 ms, activated Denim.
    pub first_block_timestamp: u64,
    /// Number of the last exported block.
    pub last_number: u64,
    /// Hash of the last exported block, which the replay must reach.
    pub last_hash: B256,
    /// State root of the last exported block, which the replay must reproduce.
    pub last_state_root: B256,
    /// Exported block count: `last_number - boundary_number`.
    pub block_count: u64,
}

/// One produced block as the engine API payload that inserts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedBlock {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Block timestamp in milliseconds, used to pace a replay.
    pub timestamp_ms: u64,
    /// Post-state root.
    pub state_root: B256,
    /// Execution payload version (1 to 4), which selects the `newPayload` method.
    pub payload_version: u8,
    /// Payload and parent beacon block root exactly as produced, including every system and
    /// deposit transaction.
    pub envelope: BaseExecutionPayloadEnvelope,
}

impl ExportedBlock {
    /// Converts a full RPC block, as returned by `eth_getBlockByNumber` with transactions, into
    /// its engine payload. Mirrors the follow client's conversion.
    pub fn from_rpc_block(block: <Base as alloy_network::Network>::BlockResponse) -> Self {
        let timestamp_ms = block
            .header
            .timestamp_ms
            .unwrap_or_else(|| block.header.timestamp.saturating_mul(1_000));
        let block = block.map_header(|header| header.into_inner());
        let hash = block.header.hash;
        let number = block.header.inner.number;
        let state_root = block.header.inner.state_root;
        let parent_beacon_block_root = block.header.inner.parent_beacon_block_root;
        let transactions: Vec<BaseTxEnvelope> =
            block.transactions.into_transactions().map(|tx| tx.inner.inner.into_inner()).collect();
        let consensus_block: Block<BaseTxEnvelope> = Block {
            header: block.header.inner,
            body: BlockBody { transactions, ommers: Vec::new(), withdrawals: block.withdrawals },
        };
        let (execution_payload, _sidecar) =
            BaseExecutionPayload::from_block_unchecked(hash, &consensus_block);
        let payload_version = match execution_payload {
            BaseExecutionPayload::V1(_) => 1,
            BaseExecutionPayload::V2(_) => 2,
            BaseExecutionPayload::V3(_) => 3,
            BaseExecutionPayload::V4(_) => 4,
        };
        Self {
            number,
            hash,
            timestamp_ms,
            state_root,
            payload_version,
            envelope: BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload },
        }
    }
}

/// Writes an export directory block by block.
#[derive(Debug)]
pub struct BlockExportWriter {
    dir: PathBuf,
    blocks: BufWriter<File>,
    last: Option<(u64, B256, B256)>,
    count: u64,
}

impl BlockExportWriter {
    /// Creates `dir` and starts a new blocks file, replacing a previous export there.
    pub fn create(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .wrap_err_with(|| format!("failed to create export directory {}", dir.display()))?;
        let _ = fs::remove_file(dir.join(EXPORT_MANIFEST_FILE));
        let path = dir.join(EXPORT_BLOCKS_FILE);
        let file =
            File::create(&path).wrap_err_with(|| format!("failed to create {}", path.display()))?;
        Ok(Self { dir: dir.to_path_buf(), blocks: BufWriter::new(file), last: None, count: 0 })
    }

    /// Appends the next block; blocks must be consecutive.
    pub fn write(&mut self, block: &ExportedBlock) -> Result<()> {
        if let Some((number, _, _)) = self.last {
            ensure!(
                block.number == number + 1,
                "export blocks must be consecutive: {} after {number}",
                block.number
            );
        }
        serde_json::to_writer(&mut self.blocks, block)
            .wrap_err("failed to encode exported block")?;
        self.blocks.write_all(b"\n").wrap_err("failed to write exported block")?;
        self.last = Some((block.number, block.hash, block.state_root));
        self.count += 1;
        Ok(())
    }

    /// Flushes the blocks and writes the manifest, completing the export. The manifest's
    /// last-block fields and count are filled from the written blocks.
    pub fn finish(mut self, mut manifest: ReplayManifest) -> Result<ReplayManifest> {
        let (last_number, last_hash, last_state_root) =
            self.last.ok_or_else(|| eyre::eyre!("no blocks were exported"))?;
        ensure!(
            last_number == manifest.boundary_number + self.count,
            "exported blocks {}..={last_number} do not start right after boundary {}",
            last_number + 1 - self.count,
            manifest.boundary_number
        );
        self.blocks.flush().wrap_err("failed to flush exported blocks")?;
        self.blocks.get_ref().sync_all().wrap_err("failed to sync exported blocks")?;
        manifest.last_number = last_number;
        manifest.last_hash = last_hash;
        manifest.last_state_root = last_state_root;
        manifest.block_count = self.count;
        let temp = self.dir.join(format!("{EXPORT_MANIFEST_FILE}.tmp"));
        fs::write(&temp, serde_json::to_vec_pretty(&manifest)?)
            .wrap_err_with(|| format!("failed to write {}", temp.display()))?;
        fs::rename(&temp, self.dir.join(EXPORT_MANIFEST_FILE))
            .wrap_err("failed to publish export manifest")?;
        Ok(manifest)
    }
}

/// Reads the manifest of a complete export.
pub fn read_export_manifest(dir: &Path) -> Result<ReplayManifest> {
    let path = dir.join(EXPORT_MANIFEST_FILE);
    let bytes = fs::read(&path).wrap_err_with(|| {
        format!("missing or unreadable {}: incomplete export?", path.display())
    })?;
    serde_json::from_slice(&bytes).wrap_err_with(|| format!("invalid {}", path.display()))
}

/// Iterates the exported blocks in order.
pub fn read_exported_blocks(dir: &Path) -> Result<impl Iterator<Item = Result<ExportedBlock>>> {
    let path = dir.join(EXPORT_BLOCKS_FILE);
    let file = File::open(&path).wrap_err_with(|| format!("failed to open {}", path.display()))?;
    Ok(BufReader::new(file).lines().enumerate().map(move |(index, line)| {
        let line = line.wrap_err("failed to read exported block")?;
        serde_json::from_str(&line)
            .wrap_err_with(|| format!("invalid exported block on line {}", index + 1))
    }))
}
