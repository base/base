//! `archive-writer`: publishes finalized chain history from Base nodes into an object-store
//! archive, retires old segments and collects orphaned objects.

use std::{path::Path, sync::Arc, time::Duration};

use alloy_genesis::Genesis;
use base_execution_chainspec::BaseChainSpec;
use base_history_store::{
    ArchiveService, ArchiveStore, ArchiveWriter, ChainIdentity, ChainSource, ChunkVerifier,
    DEFAULT_ORPHAN_MIN_AGE, DEFAULT_RETIREMENT_GRACE, IndexOptions, MaintenanceConfig,
    RpcChainSource, SourcePool, WriterConfig,
};
use clap::{Parser, Subcommand};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,

    /// Chain name (for example `base`, `base-sepolia`) or path to a genesis JSON file.
    #[arg(long, env = "ARCHIVE_CHAIN", global = true, default_value = "base")]
    pub chain: String,

    /// Archive location: `s3://bucket/prefix` or `memory://` (catalog CAS needs one of these).
    #[arg(long = "archive.url", env = "ARCHIVE_URL", global = true)]
    pub archive_url: Option<String>,

    /// S3 service endpoint for S3-compatible stores such as `MinIO`.
    #[arg(long = "archive.endpoint", env = "ARCHIVE_ENDPOINT", global = true)]
    pub archive_endpoint: Option<String>,

    /// Node RPC URLs serving `archive_`; requests rotate between them.
    #[arg(long = "source", env = "ARCHIVE_SOURCES", value_delimiter = ',', global = true)]
    pub sources: Vec<String>,

    /// Timeout of one source request.
    #[arg(long, default_value = "30s", value_parser = humantime::parse_duration, global = true)]
    pub source_timeout: Duration,

    /// Blocks per segment `N`, fixed when the catalog is created.
    #[arg(long, default_value_t = 10_000, global = true, value_parser = clap::value_parser!(u64).range(1..))]
    pub blocks_per_segment: u64,

    /// Blocks per `archive_getBlockRange` call; clamped to every source's advertised limit and
    /// halved when a source rejects a range as too large.
    #[arg(long, default_value_t = 1024, global = true, value_parser = clap::value_parser!(u64).range(1..))]
    pub chunk_blocks: u64,

    /// Most segments retired, and most swept, per maintenance pass.
    #[arg(long, default_value_t = base_history_store::DEFAULT_MAX_SEGMENTS_PER_PASS, global = true)]
    pub max_segments_per_pass: usize,

    /// Retire segments whose last block is older than this (for example `90d`); unset keeps all.
    #[arg(long, value_parser = humantime::parse_duration, global = true)]
    pub retention: Option<Duration>,

    /// Delay between retiring a segment and deleting its objects.
    #[arg(long, value_parser = humantime::parse_duration, global = true)]
    pub retirement_grace: Option<Duration>,

    /// Minimum age of an orphaned object above `H` before deletion.
    #[arg(long, value_parser = humantime::parse_duration, global = true)]
    pub orphan_min_age: Option<Duration>,

    /// Time between maintenance passes in `run`.
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration, global = true)]
    pub maintenance_interval: Duration,
}

/// Subcommands.
#[derive(Debug, Clone, Copy, Subcommand)]
pub enum Command {
    /// Publish segments as they finalize and run maintenance periodically.
    Run,
    /// Delete orphaned segment objects above `H` once. Claims the archive, so it fences a
    /// running `run` instance; use it only while the service is stopped.
    Gc,
    /// Retire old segments and sweep retired ones past the grace period once. Claims the
    /// archive, so it fences a running `run` instance; use it only while the service is stopped.
    Prune,
}

impl Args {
    /// Resolves `--chain` to a chain spec.
    pub fn chain_spec(&self) -> eyre::Result<Arc<BaseChainSpec>> {
        if let Some(spec) = BaseChainSpec::parse_chain(&self.chain) {
            return Ok(spec);
        }
        let genesis: Genesis = serde_json::from_str(&std::fs::read_to_string(Path::new(&self.chain))?)?;
        Ok(Arc::new(BaseChainSpec::try_from_genesis(genesis)?))
    }

    /// Maintenance settings from the flags.
    pub fn maintenance(&self) -> MaintenanceConfig {
        MaintenanceConfig {
            retention: self.retention,
            retirement_grace: self.retirement_grace.unwrap_or(DEFAULT_RETIREMENT_GRACE),
            orphan_min_age: self.orphan_min_age.unwrap_or(DEFAULT_ORPHAN_MIN_AGE),
            interval: self.maintenance_interval,
            max_segments_per_pass: self.max_segments_per_pass,
        }
    }

    /// Opens the writer service.
    pub async fn service(&self) -> eyre::Result<ArchiveService> {
        let url = self.archive_url.as_deref().ok_or_else(|| eyre::eyre!("--archive.url is required"))?;
        let archive = ArchiveStore::from_url(url, self.archive_endpoint.as_deref())?;
        let chain_spec = self.chain_spec()?;
        let sources = self
            .sources
            .iter()
            .map(|url| {
                RpcChainSource::new(url.clone(), self.source_timeout)
                    .map(|source| Arc::new(source) as Arc<dyn ChainSource>)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let chain = ChainIdentity {
            chain_id: chain_spec.inner.chain.id(),
            genesis_hash: chain_spec.inner.genesis_hash(),
            blocks_per_segment: self.blocks_per_segment,
        };
        let writer = ArchiveWriter::open(
            archive.store,
            archive.layout,
            chain,
            SourcePool::new(sources),
            ChunkVerifier::new(chain_spec),
            WriterConfig { chunk_blocks: self.chunk_blocks, ..WriterConfig::default() },
            IndexOptions::default(),
        )
        .await?;
        Ok(ArchiveService::new(writer, self.maintenance()))
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    let args = Args::parse();
    if matches!(args.command, Command::Gc | Command::Prune) {
        warn!("claiming the archive: any running archive-writer instance is fenced and stops");
    }
    let service = args.service().await?;
    let result = match args.command {
        Command::Run => tokio::select! {
            result = service.run() => result.map_err(Into::into),
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down archive writer");
                Ok(())
            }
        },
        Command::Gc => {
            let report = service.collect_orphans().await?;
            info!(deleted = report.deleted.len(), kept_young = report.kept_young.len(), "orphan gc");
            Ok(())
        }
        Command::Prune => {
            let report = service.maintain().await?;
            info!(retired = ?report.retired, swept = ?report.swept, "archive prune");
            Ok(())
        }
    };
    service.writer.index.close().await?;
    result
}
