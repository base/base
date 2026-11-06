//! `base-bench export`: canonical blocks read from a node datadir, written in the format
//! `base-bench replay` reads. A replay target must hold the block before `--from`.

use std::{path::PathBuf, sync::Arc};

use alloy_primitives::B256;
use base_common_chains::Upgrades;
use base_node_core::BaseNode;
use clap::Args;
use eyre::{OptionExt, Result, ensure};
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, HeaderProvider, providers::ReadOnlyConfig,
};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};

use crate::{BlockExportWriter, ExportSource, ExportedBlock, ReplayManifest, SnapshotChainConfig};

/// Blocks read per database transaction, so no read outlives MDBX's long-read timeout.
const CHUNK_BLOCKS: u64 = 256;

/// Arguments for exporting canonical blocks from a node datadir for `base-bench replay`.
#[derive(Debug, Args)]
pub struct ExportBenchmarkArgs {
    /// Built-in Base chain name or path to a Base genesis JSON file.
    #[arg(long, default_value = "mainnet")]
    pub chain: String,
    /// Rollup config JSON for a custom chain JSON whose chain ID is not built in.
    #[arg(long)]
    pub rollup_config: Option<PathBuf>,
    /// Node datadir to read blocks from, opened read-only.
    #[arg(long)]
    pub datadir: PathBuf,
    /// First exported block.
    #[arg(long)]
    pub from: u64,
    /// Last exported block; defaults to the datadir head.
    #[arg(long)]
    pub to: Option<u64>,
    /// Directory for `blocks.jsonl` and `replay.json`.
    #[arg(long)]
    pub output_dir: PathBuf,
}

impl ExportBenchmarkArgs {
    /// Exports blocks `--from..=--to` and writes the replay manifest.
    pub async fn run(self) -> Result<()> {
        let chain_spec = SnapshotChainConfig {
            chain: self.chain.clone(),
            rollup_config: self.rollup_config.clone(),
        }
        .resolve()?
        .chain_spec;
        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
        )
        .build()?;
        let factory = BaseNode::provider_factory_builder().open_read_only(
            Arc::clone(&chain_spec),
            ReadOnlyConfig::from_datadir(&self.datadir).no_watch(),
            runtime,
        )?;
        let provider = factory.provider()?;
        if let Some(genesis_hash) = provider.block_hash(0)? {
            ensure!(
                genesis_hash == chain_spec.genesis_hash(),
                "datadir genesis {genesis_hash} does not match chain {}",
                self.chain
            );
        }
        let head_number = provider.best_block_number()?;
        let head = provider.header_by_number(head_number)?.ok_or_eyre("datadir head is missing")?;
        let to = self.to.unwrap_or(head_number);
        ensure!(
            0 < self.from && self.from <= to && to <= head_number,
            "range {}..={to} is not within the datadir's blocks 1..={head_number}",
            self.from
        );
        let boundary_number = self.from - 1;
        let boundary_hash =
            provider.block_hash(boundary_number)?.ok_or_eyre("boundary block is missing")?;
        drop(provider);

        let mut writer = BlockExportWriter::create(&self.output_dir)?;
        let mut timestamps = None;
        for start in (self.from..=to).step_by(CHUNK_BLOCKS as usize) {
            let end = to.min(start + CHUNK_BLOCKS - 1);
            let blocks = factory.provider()?.block_range(start..=end)?;
            ensure!(
                blocks.len() as u64 == end - start + 1,
                "datadir is missing blocks in {start}..={end}"
            );
            for block in &blocks {
                let block = ExportedBlock::from_stored_block(block);
                let timestamp = block.envelope.execution_payload.timestamp();
                timestamps.get_or_insert((timestamp, timestamp)).1 = timestamp;
                writer.write(&block)?;
            }
        }
        let (first_block_timestamp, last_block_timestamp) = timestamps.unwrap_or_default();
        // A manifest has one block interval, which replay applies to every block.
        let subsecond = chain_spec.is_denim_active_at_timestamp(last_block_timestamp);
        ensure!(
            chain_spec.is_denim_active_at_timestamp(first_block_timestamp) == subsecond,
            "blocks {}..={to} span Denim activation; export each side separately",
            self.from
        );
        writer.finish(ReplayManifest {
            chain: self.chain,
            rollup_config: self.rollup_config,
            block_interval_ms: if subsecond { 200 } else { 2_000 },
            boundary_number,
            boundary_hash,
            first_block_timestamp,
            last_number: 0,
            last_hash: B256::ZERO,
            last_state_root: B256::ZERO,
            block_count: 0,
            source: Some(ExportSource {
                datadir: self.datadir,
                head_number,
                head_timestamp: head.timestamp,
            }),
        })?;
        Ok(())
    }
}
