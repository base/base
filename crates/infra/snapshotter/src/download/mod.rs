//! Snapshot download command: fetch, extract, and verify Base snapshots from R2.

mod extract;
pub use extract::ArchiveExtractor;

mod fetch;
pub use fetch::ArchiveFetcher;

mod plan;
pub use plan::{
    ChunkedComponentManifest, DownloadComponent, DownloadPlanner, PlannedArchive, PlannedDownloads,
    SelectionPreset, SingleComponentManifest,
};

mod progress;
pub use progress::{DownloadProgressTracker, format_size};

mod verify;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{self, StreamExt, TryStreamExt};
use tracing::{info, warn};
pub use verify::OutputVerifier;

use crate::SnapshotManifest;

/// Default Base snapshot R2 base URL.
const DEFAULT_BASE_URL: &str = "https://snapshots.base.org";

/// Maximum number of concurrent archive downloads.
const MAX_CONCURRENT_DOWNLOADS: usize = 4;

/// Download Base node snapshots from R2 storage.
///
/// Fetches a snapshot manifest, selects components based on the chosen preset,
/// and downloads, extracts, and verifies each archive.
///
/// # Examples
///
/// Download a full archive node with proofs:
/// ```text
/// base-node download --preset proofs --datadir /data/reth
/// ```
///
/// Resume an interrupted download:
/// ```text
/// base-node download --preset archive --datadir /data/reth
/// ```
#[derive(Debug, Parser)]
#[command(name = "download", about = "Download Base node snapshots from R2 storage")]
pub struct BaseDownloadCommand {
    /// Component selection preset.
    #[arg(long, default_value = "full")]
    pub preset: SelectionPreset,

    /// Target data directory for extracted snapshot data.
    #[arg(long, short = 'd')]
    pub datadir: PathBuf,

    /// Direct URL to the snapshot manifest JSON.
    ///
    /// If not provided, the latest manifest is fetched from the default
    /// Base snapshot endpoint.
    #[arg(long)]
    pub manifest_url: Option<String>,

    /// Base URL for the snapshot storage bucket.
    #[arg(long, default_value = DEFAULT_BASE_URL)]
    pub base_url: String,

    /// Bucket key prefix (e.g. `mainnet`, `sepolia`).
    #[arg(long, default_value = "mainnet")]
    pub prefix: String,

    /// Snapshot run timestamp directory. If not provided, the latest is used.
    #[arg(long)]
    pub run_timestamp: Option<u64>,

    /// Maximum number of concurrent archive downloads.
    #[arg(long, default_value_t = MAX_CONCURRENT_DOWNLOADS)]
    pub concurrency: usize,

    /// Local cache directory for downloaded archives before extraction.
    ///
    /// Defaults to `{datadir}/.snapshot-cache`.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Skip verification after extraction.
    #[arg(long, default_value_t = false)]
    pub skip_verify: bool,
}

impl BaseDownloadCommand {
    /// Executes the download pipeline.
    pub async fn execute(self) -> Result<()> {
        let cache_dir =
            self.cache_dir.clone().unwrap_or_else(|| self.datadir.join(".snapshot-cache"));
        tokio::fs::create_dir_all(&cache_dir)
            .await
            .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;
        tokio::fs::create_dir_all(&self.datadir)
            .await
            .with_context(|| format!("failed to create datadir {}", self.datadir.display()))?;

        let manifest = self.fetch_manifest().await?;

        info!(
            block = manifest.block,
            chain_id = manifest.chain_id,
            components = manifest.components.len(),
            "loaded snapshot manifest"
        );

        let run_prefix = match self.run_timestamp {
            Some(ts) => {
                if self.prefix.is_empty() {
                    ts.to_string()
                } else {
                    format!("{}/{ts}", self.prefix)
                }
            }
            None => {
                if self.prefix.is_empty() {
                    manifest.timestamp.to_string()
                } else {
                    format!("{}/{}", self.prefix, manifest.timestamp)
                }
            }
        };

        let static_files_prefix = if self.prefix.is_empty() {
            "static_files".to_string()
        } else {
            format!("{}/static_files", self.prefix)
        };

        let plan = DownloadPlanner::plan(
            &manifest,
            self.preset,
            &self.base_url,
            &run_prefix,
            &static_files_prefix,
        )?;

        if plan.archives.is_empty() {
            info!("nothing to download");
            return Ok(());
        }

        info!(
            archives = plan.total_archives,
            download_size = %format_size(plan.total_download_size),
            preset = ?self.preset,
            "starting download"
        );

        self.download_and_extract(plan, &cache_dir).await?;

        if let Err(e) = tokio::fs::remove_dir_all(&cache_dir).await {
            warn!(error = %e, "failed to clean up cache directory");
        }

        info!(datadir = %self.datadir.display(), "download complete");
        Ok(())
    }

    /// Fetches the snapshot manifest from the configured URL.
    async fn fetch_manifest(&self) -> Result<SnapshotManifest> {
        let url = match &self.manifest_url {
            Some(url) => url.clone(),
            None => {
                let ts = self
                    .run_timestamp
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "latest".to_string());
                if self.prefix.is_empty() {
                    format!("{}/{ts}/manifest.json", self.base_url)
                } else {
                    format!("{}/{}/{ts}/manifest.json", self.base_url, self.prefix)
                }
            }
        };

        info!(url = %url, "fetching manifest");

        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to fetch manifest from {url}"))?;

        if !response.status().is_success() {
            anyhow::bail!("manifest fetch returned HTTP {}: {url}", response.status());
        }

        let manifest: SnapshotManifest = response
            .json()
            .await
            .with_context(|| format!("failed to parse manifest from {url}"))?;

        Ok(manifest)
    }

    /// Downloads, extracts, and verifies all planned archives.
    async fn download_and_extract(&self, plan: PlannedDownloads, cache_dir: &Path) -> Result<()> {
        let fetcher = ArchiveFetcher::new()?;
        let tracker = DownloadProgressTracker::new();
        let summary_bar = tracker.add_summary_bar(plan.total_archives as u64);
        let datadir = self.datadir.clone();
        let skip_verify = self.skip_verify;
        let concurrency = self.concurrency;

        stream::iter(plan.archives)
            .map(|archive| {
                let fetcher = &fetcher;
                let tracker = &tracker;
                let datadir = &datadir;
                let cache_dir = cache_dir;
                let summary_bar = &summary_bar;

                async move {
                    Self::process_archive(
                        fetcher,
                        tracker,
                        &archive,
                        cache_dir,
                        datadir,
                        skip_verify,
                    )
                    .await?;

                    summary_bar.inc(1);
                    Ok::<_, anyhow::Error>(())
                }
            })
            .buffer_unordered(concurrency)
            .try_collect::<Vec<()>>()
            .await?;

        summary_bar.finish_with_message("all archives complete");
        Ok(())
    }

    /// Processes a single archive: check existing → download → extract → verify.
    async fn process_archive(
        fetcher: &ArchiveFetcher,
        tracker: &DownloadProgressTracker,
        archive: &PlannedArchive,
        cache_dir: &Path,
        datadir: &Path,
        skip_verify: bool,
    ) -> Result<()> {
        if !archive.output_files.is_empty() {
            let already_valid = tokio::task::spawn_blocking({
                let output_files = archive.output_files.clone();
                let verifier_dir = datadir.to_path_buf();
                move || OutputVerifier::new(&verifier_dir).verify(&output_files)
            })
            .await
            .context("verification task panicked")??;

            if already_valid {
                info!(archive = %archive.file_name, "already verified, skipping");
                return Ok(());
            }
        }

        let download_bar = tracker.add_download_bar(&archive.file_name, archive.size);
        let archive_path = fetcher
            .download_to_cache(&archive.url, cache_dir, Some(&download_bar))
            .await
            .with_context(|| format!("failed to download {}", archive.file_name))?;
        download_bar.finish_with_message("downloaded");

        let extract_spinner = tracker.add_spinner(&format!("extracting {}", archive.file_name));
        let extract_target = datadir.to_path_buf();
        let extract_path = archive_path.clone();
        tokio::task::spawn_blocking(move || {
            ArchiveExtractor::extract(&extract_path, &extract_target, None)
        })
        .await
        .context("extraction task panicked")??;
        extract_spinner.finish_with_message("extracted");

        tokio::fs::remove_file(&archive_path).await.ok();

        if !skip_verify && !archive.output_files.is_empty() {
            let verify_spinner = tracker.add_spinner(&format!("verifying {}", archive.file_name));
            let output_files = archive.output_files.clone();
            let verify_dir = datadir.to_path_buf();

            let verified = tokio::task::spawn_blocking(move || {
                OutputVerifier::new(&verify_dir).verify(&output_files)
            })
            .await
            .context("verification task panicked")??;

            if !verified {
                verify_spinner.finish_with_message("FAILED");
                anyhow::bail!(
                    "verification failed for {} — extracted files do not match manifest checksums",
                    archive.file_name
                );
            }
            verify_spinner.finish_with_message("verified");
        }

        Ok(())
    }
}
