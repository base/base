//! `archive-writer`: publishes finalized chain history from Base nodes into an object-store
//! archive, retires old segments and collects orphaned objects.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy_genesis::Genesis;
use base_execution_chainspec::BaseChainSpec;
use base_history_store::{
    ArchiveService, ArchiveStore, ArchiveWriter, ChainIdentity, ChainSource, ChunkVerifier,
    DEFAULT_ORPHAN_MIN_AGE, DEFAULT_RETIREMENT_GRACE, IndexOptions, MaintenanceConfig,
    RpcChainSource, SourcePool, WriterConfig,
};
use clap::{Parser, Subcommand};
use reth_rpc_layer::{JwtError, JwtSecret};
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

    /// Node authenticated RPC URLs (`--authrpc.addr`/`--authrpc.port`) serving `archive_`;
    /// requests rotate between them.
    #[arg(long = "source", env = "ARCHIVE_SOURCES", value_delimiter = ',', global = true)]
    pub sources: Vec<String>,

    /// File holding the hex JWT secret the source nodes use for `--authrpc.jwtsecret`. Without
    /// it requests are unauthenticated, which only nodes exposing `archive_` on IPC-bridged or
    /// test transports accept.
    #[arg(long = "archive.jwt-secret", value_name = "PATH", global = true)]
    pub jwt_secret: Option<PathBuf>,

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
        let genesis: Genesis =
            serde_json::from_str(&std::fs::read_to_string(Path::new(&self.chain))?)?;
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

    /// Builds one source per `--source`, signing requests with `--archive.jwt-secret` when set.
    pub fn sources(&self) -> eyre::Result<Vec<Arc<dyn ChainSource>>> {
        let secret = match &self.jwt_secret {
            // Only read failures are described; decode errors can quote secret characters.
            Some(path) => Some(JwtSecret::from_file(path).map_err(|err| match err {
                JwtError::Read { source, .. } => {
                    eyre::eyre!("reading --archive.jwt-secret {}: {source}", path.display())
                }
                _ => eyre::eyre!(
                    "--archive.jwt-secret {} is not a 32-byte hex secret",
                    path.display()
                ),
            })?),
            None => {
                warn!("no --archive.jwt-secret; source requests are unauthenticated");
                None
            }
        };
        let timeout = self.source_timeout;
        let sources = self
            .sources
            .iter()
            .map(|url| -> eyre::Result<Arc<dyn ChainSource>> {
                Ok(match secret {
                    Some(secret) => {
                        Arc::new(RpcChainSource::with_jwt(url.clone(), timeout, secret)?)
                    }
                    None => Arc::new(RpcChainSource::new(url.clone(), timeout)?),
                })
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        Ok(sources)
    }

    /// Opens the writer service.
    pub async fn service(&self) -> eyre::Result<ArchiveService> {
        let url =
            self.archive_url.as_deref().ok_or_else(|| eyre::eyre!("--archive.url is required"))?;
        let archive = ArchiveStore::from_url(url, self.archive_endpoint.as_deref())?;
        let chain_spec = self.chain_spec()?;
        let sources = self.sources()?;
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
            info!(
                deleted = report.deleted.len(),
                kept_young = report.kept_young.len(),
                "orphan gc"
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `contents` to a fresh file under the temp dir and returns its path.
    fn secret_file(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("archive-writer-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).expect("write secret file");
        path
    }

    /// `--archive.jwt-secret` builds authenticated sources; a malformed secret fails without
    /// quoting the file contents.
    #[test]
    fn jwt_secret_flag_builds_sources_and_never_echoes_the_secret() {
        let good = secret_file("good", &"ab".repeat(32));
        let args = Args::try_parse_from([
            "archive-writer",
            "--source",
            "http://127.0.0.1:8551,http://127.0.0.1:8552",
            "--archive.jwt-secret",
            good.to_str().expect("utf-8 path"),
            "run",
        ])
        .expect("flags parse");
        assert_eq!(args.sources().expect("sources build").len(), 2, "one source per url");

        let secret = "zz-not-hex-secret-material-zz";
        let bad = secret_file("bad", secret);
        let args = Args::try_parse_from([
            "archive-writer",
            "--source",
            "http://127.0.0.1:8551",
            "--archive.jwt-secret",
            bad.to_str().expect("utf-8 path"),
            "run",
        ])
        .expect("flags parse");
        let err = args.sources().err().expect("a malformed secret is rejected").to_string();
        assert!(err.contains("is not a 32-byte hex secret"), "names the problem: {err}");
        assert!(!err.contains("zz"), "never quotes the secret: {err}");
        let _ = std::fs::remove_file(good);
        let _ = std::fs::remove_file(bad);
    }
}
