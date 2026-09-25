//! Download command wrapper that extends reth's `DownloadCommand` with `--proofs`.
//!
//! Delegates all standard snapshot components to reth's download pipeline and
//! handles the Base-specific proofs database download separately using the
//! same snapshot source and manifest.

use std::{
    ffi::OsString,
    io::{BufReader, Read, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base_execution_chainspec::BaseChainSpec;
use base_reth_cli::{ProgressDisplay, ZstdArchiveReader};
use clap::Parser;
use eyre::Result;
use futures::{StreamExt, future::try_join_all};
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::download::{DownloadCommand, DownloadDefaults};
use reth_node_core::args::DatadirArgs;
use tokio::{
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::Mutex,
};
use tracing::{debug, info};

/// Download Base node snapshots from R2 storage.
///
/// Wraps reth's download command with an additional `--proofs` flag that
/// downloads the expanded trie proof database for fault proof support.
///
/// When `--proofs` is passed, the command runs reth's standard download
/// then fetches and extracts the proofs archive from the same snapshot source.
#[derive(Debug, Parser)]
pub struct BaseDownloadCommand<C: ChainSpecParser> {
    #[command(flatten)]
    inner: DownloadCommand<C>,

    /// Also download the proofs database for fault proof support.
    ///
    /// After the standard download completes, fetches the proofs archive
    /// from the same snapshot source and extracts it into the data directory.
    /// Re-running with `--proofs` will overwrite any existing proofs database.
    #[arg(long)]
    proofs: bool,
}

impl<C: ChainSpecParser<ChainSpec = BaseChainSpec>> BaseDownloadCommand<C> {
    /// Executes the download command.
    pub async fn execute<N>(self) -> Result<()> {
        let Self { inner, proofs } = self;

        let (data_dir, chain_id) = if proofs {
            let chain = inner
                .chain_spec()
                .ok_or_else(|| eyre::eyre!("--proofs flag is only on Base"))?
                .chain();
            let chain_id = chain.id();
            let dir = resolve_datadir_args(std::env::args_os()).resolve_datadir(chain);
            info!(target: "reth::cli", datadir = %dir.data_dir().display(), "Resolved datadir for proofs download");
            (Some(dir), Some(chain_id))
        } else {
            (None, None)
        };

        inner.execute::<N>().await?;

        if let (Some(data_dir), Some(chain_id)) = (data_dir, chain_id) {
            let target_dir = data_dir.data_dir().to_path_buf();
            let concurrency = resolve_download_concurrency_arg(std::env::args_os());
            ProofsDownloader::run(&target_dir, chain_id, concurrency).await?;
        }

        Ok(())
    }
}

/// Extracts `--datadir` from the current process args without doing a second
/// permissive clap parse of the whole command.
fn resolve_datadir_args(args: impl IntoIterator<Item = OsString>) -> DatadirArgs {
    let mut datadir_args = DatadirArgs::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };

        if arg == "--datadir" {
            if let Some(value) = args.next() {
                datadir_args.datadir = PathBuf::from(value).into();
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--datadir=") {
            datadir_args.datadir = PathBuf::from(value).into();
        }
    }

    datadir_args
}

/// Matches reth's `--download-concurrency` default.
const DEFAULT_DOWNLOAD_CONCURRENCY: usize = 8;

/// Interval between proofs download and extraction progress logs.
const PROOFS_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(3);

/// Extracts `--download-concurrency` so proofs use the same parallel budget as reth.
fn resolve_download_concurrency_arg(args: impl IntoIterator<Item = OsString>) -> usize {
    let mut concurrency = DEFAULT_DOWNLOAD_CONCURRENCY;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };

        if arg == "--download-concurrency" {
            if let Some(value) = args.next()
                && let Ok(parsed) = value.to_string_lossy().parse::<usize>()
            {
                concurrency = parsed;
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--download-concurrency=")
            && let Ok(parsed) = value.parse::<usize>()
        {
            concurrency = parsed;
        }
    }

    concurrency.max(1)
}

/// Splits `total` bytes into `parts` contiguous half-open ranges.
fn split_byte_ranges(total: u64, parts: usize) -> Vec<std::ops::Range<u64>> {
    let parts = parts.max(1);
    if total == 0 {
        return Vec::new();
    }

    let parts = parts.min(total as usize);
    let chunk = total / parts as u64;
    let rem = total % parts as u64;
    let mut ranges = Vec::with_capacity(parts);
    let mut start = 0u64;
    for i in 0..parts {
        let extra = u64::from((i as u64) < rem);
        let end = start + chunk + extra;
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// Writes parallel-range resume state next to the `.part` file.
async fn persist_range_sidecar(
    path: &Path,
    concurrency: usize,
    expected_size: u64,
    written: &[u64],
) -> Result<()> {
    let mut line = format!("{concurrency} {expected_size}");
    for amount in written {
        line.push(' ');
        line.push_str(&amount.to_string());
    }
    tokio::fs::write(path, line).await?;
    Ok(())
}

/// Delay before an idle proofs download retry.
///
/// Stream drops stay at 1s. HTTP 429/5xx use exponential backoff so a
/// rate-limiting CDN is not hammered.
fn retry_delay(idle_attempts: u32, throttled: bool) -> Duration {
    if !throttled {
        return Duration::from_secs(1);
    }

    let multiplier = 1u64 << idle_attempts.saturating_sub(1).min(4);
    Duration::from_secs((2 * multiplier).min(30))
}

/// Logs proofs download progress using the same fields as snapshot compression.
fn log_proofs_download_progress(downloaded: u64, expected: u64, started: Instant, baseline: u64) {
    let elapsed = started.elapsed();
    let session_done = downloaded.saturating_sub(baseline);
    let session_total = expected.saturating_sub(baseline);
    let speed = session_done as f64 / elapsed.as_secs_f64();
    let eta = ProgressDisplay::eta(session_done, session_total, elapsed)
        .map_or_else(|| "unknown".to_string(), |eta| eta.to_string());
    info!(
        target: "reth::cli",
        progress = %ProgressDisplay::human_byte_progress(downloaded, expected),
        speed = %ProgressDisplay::speed(speed),
        eta = %eta,
        elapsed = %ProgressDisplay::duration(elapsed),
        "Proofs download progress"
    );
}

/// Reports extraction progress as compressed archive bytes are consumed.
struct ExtractionProgress<R> {
    inner: R,
    processed: u64,
    total: u64,
    started: Instant,
    last_log: Instant,
}

impl<R> ExtractionProgress<R> {
    fn new(inner: R, total: u64) -> Self {
        let now = Instant::now();
        Self { inner, processed: 0, total, started: now, last_log: now }
    }
}

impl<R: Read> Read for ExtractionProgress<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.processed = self.processed.saturating_add(read as u64).min(self.total);

        if read > 0 && self.last_log.elapsed() >= PROOFS_PROGRESS_LOG_INTERVAL {
            let elapsed = self.started.elapsed();
            let speed = self.processed as f64 / elapsed.as_secs_f64();
            let eta = ProgressDisplay::eta(self.processed, self.total, elapsed)
                .map_or_else(|| "unknown".to_string(), |eta| eta.to_string());
            info!(
                target: "reth::cli",
                progress = %ProgressDisplay::human_byte_progress(self.processed, self.total),
                speed = %ProgressDisplay::speed(speed),
                eta = %eta,
                elapsed = %ProgressDisplay::duration(elapsed),
                "Proofs extraction progress"
            );
            self.last_log = Instant::now();
        }

        Ok(read)
    }
}

/// Loads parallel-range resume state when it matches this download.
fn load_range_sidecar(path: &Path, concurrency: usize, expected_size: u64) -> Option<Vec<u64>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut parts = text.split_whitespace();
    let stored_concurrency = parts.next()?.parse::<usize>().ok()?;
    let stored_expected = parts.next()?.parse::<u64>().ok()?;
    if stored_concurrency != concurrency || stored_expected != expected_size {
        return None;
    }

    let written = parts.map(|part| part.parse().ok()).collect::<Option<Vec<u64>>>()?;
    (written.len() == concurrency).then_some(written)
}

/// Trusts a sidecar only when the `.part` file is already the declared size.
///
/// A missing or resized `.part` means the sidecar describes bytes that are
/// no longer on disk, so resume must start from zero.
fn load_trusted_range_sidecar(
    path: &Path,
    concurrency: usize,
    expected_size: u64,
    part_len: u64,
) -> Option<Vec<u64>> {
    if part_len != expected_size {
        return None;
    }
    load_range_sidecar(path, concurrency, expected_size)
}

/// Extracts `--manifest-url` so proofs reuse the same snapshot reth downloaded.
fn resolve_manifest_url_arg(args: impl IntoIterator<Item = OsString>) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };

        if arg == "--manifest-url" {
            return args.next().and_then(|value| value.into_string().ok());
        }

        if let Some(value) = arg.strip_prefix("--manifest-url=") {
            return Some(value.to_string());
        }
    }

    None
}

/// Reads a snapshot API field that may be a JSON number or a numeric string.
fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    match value {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// Discovers the latest modular snapshot manifest URL for `chain_id`.
///
/// Reth's download pipeline queries the snapshot API (`metadataUrl`) rather than
/// concatenating `{default_base_url}/{chain_id}/manifest.json`. Each chain publishes
/// manifests under its own bucket (`https://zeronet-v2-snapshots.base.org/...`), so
/// deriving a URL from the snapshot root instead produces 404s.
async fn discover_latest_manifest_url(api_url: &str, chain_id: u64) -> Result<String> {
    info!(
        target: "reth::cli",
        api_url = %api_url,
        chain_id,
        "Discovering latest snapshot manifest for proofs"
    );

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let listing: serde_json::Value = client
        .get(api_url)
        .send()
        .await
        .map_err(|e| eyre::eyre!("failed to fetch snapshot listing from {api_url}: {e}"))?
        .error_for_status()
        .map_err(|e| eyre::eyre!("failed to fetch snapshot listing from {api_url}: {e}"))?
        .json()
        .await
        .map_err(|e| eyre::eyre!("failed to parse snapshot listing from {api_url}: {e}"))?;

    let entries = listing
        .as_array()
        .ok_or_else(|| eyre::eyre!("snapshot listing from {api_url} is not a JSON array"))?;

    let (block, metadata_url) = entries
        .iter()
        .filter_map(|entry| {
            let id = json_u64(entry.get("chainId"))?;
            if id != chain_id {
                return None;
            }
            let metadata_url = entry.get("metadataUrl").and_then(|v| v.as_str())?;
            if !metadata_url.ends_with("manifest.json") {
                return None;
            }
            let block = json_u64(entry.get("block"))?;
            Some((block, metadata_url.to_string()))
        })
        .max_by_key(|(block, _)| *block)
        .ok_or_else(|| {
            eyre::eyre!("no modular snapshot manifest found for chain {chain_id} at {api_url}")
        })?;

    info!(
        target: "reth::cli",
        block,
        url = %metadata_url,
        "Found latest snapshot manifest for proofs"
    );

    Ok(metadata_url)
}

impl<C: ChainSpecParser> BaseDownloadCommand<C> {
    /// Returns the underlying chain spec.
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        self.inner.chain_spec()
    }
}

/// Metadata parsed from the manifest's `proofs` component.
#[derive(Debug)]
struct ProofsManifestEntry {
    file_name: String,
    expected_size: u64,
    archive_url: String,
}

/// Consecutive Range-resume attempts that write no new bytes before failing.
///
/// Resets on progress so a multi-hundred-GiB Cloudflare GET can drop many
/// times without aborting the process.
const MAX_IDLE_DOWNLOAD_ATTEMPTS: u32 = 8;

/// Idle read timeout so a stalled CDN socket fails into retry instead of hanging.
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Shared write-progress for one parallel proofs download.
#[derive(Clone)]
struct RangeProgress {
    sidecar_path: PathBuf,
    concurrency: usize,
    expected_size: u64,
    written: Arc<Mutex<Vec<u64>>>,
    progress: Arc<AtomicU64>,
}

/// Downloads the proofs database from a snapshot manifest.
///
/// Encapsulates the full pipeline: manifest fetch → archive download with
/// resume → size verification → tar+zstd extraction → cache cleanup.
#[derive(Debug)]
struct ProofsDownloader;

impl ProofsDownloader {
    /// Runs the full proofs download pipeline for the given chain.
    async fn run(target_dir: &Path, chain_id: u64, concurrency: usize) -> Result<()> {
        let manifest_url = match resolve_manifest_url_arg(std::env::args_os()) {
            Some(url) => url,
            None => {
                let api_url = DownloadDefaults::get_global().snapshot_api_url.as_ref();
                discover_latest_manifest_url(api_url, chain_id).await?
            }
        };

        Self::run_from_manifest(target_dir, &manifest_url, concurrency).await
    }

    /// Runs the full proofs download pipeline from a manifest URL.
    async fn run_from_manifest(
        target_dir: &Path,
        manifest_url: &str,
        concurrency: usize,
    ) -> Result<()> {
        let entry = Self::fetch_manifest_entry(manifest_url).await?;

        let cache_dir = target_dir.join(".snapshot-cache");
        tokio::fs::create_dir_all(&cache_dir).await?;

        let archive_path = Self::download_archive(&entry, &cache_dir, concurrency).await?;

        Self::extract_and_cleanup(&archive_path, target_dir, &cache_dir).await
    }

    /// Fetches the manifest and extracts the proofs component metadata.
    async fn fetch_manifest_entry(manifest_url: &str) -> Result<ProofsManifestEntry> {
        info!(target: "reth::cli", manifest_url = %manifest_url, "Fetching manifest for proofs component");

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;

        let manifest: serde_json::Value = client
            .get(manifest_url)
            .send()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch manifest from {manifest_url}: {e}"))?
            .error_for_status()
            .map_err(|e| eyre::eyre!("failed to fetch manifest from {manifest_url}: {e}"))?
            .json()
            .await
            .map_err(|e| eyre::eyre!("failed to parse manifest from {manifest_url}: {e}"))?;

        let proofs_component =
            manifest.get("components").and_then(|c| c.get("proofs")).ok_or_else(|| {
                eyre::eyre!(
                    "manifest has no 'proofs' component — this snapshot does not include proofs"
                )
            })?;

        let file_name = proofs_component
            .get("file")
            .and_then(|f| f.as_str())
            .ok_or_else(|| eyre::eyre!("proofs component missing 'file' field in manifest"))?
            .to_string();

        let expected_size = proofs_component
            .get("size")
            .and_then(|s| s.as_u64())
            .ok_or_else(|| eyre::eyre!("proofs component missing 'size' field in manifest"))?;

        let file_path = std::path::Path::new(&file_name);
        if file_path.is_absolute()
            || file_name.contains("..")
            || file_path.components().count() != 1
        {
            eyre::bail!("invalid proofs file name in manifest: {file_name}");
        }

        let archive_base_url = manifest_url
            .rsplit_once('/')
            .map(|(base, _)| base.to_string())
            .ok_or_else(|| eyre::eyre!("malformed manifest URL: {manifest_url}"))?;

        let archive_url = format!("{archive_base_url}/{file_name}");

        Ok(ProofsManifestEntry { file_name, expected_size, archive_url })
    }

    /// Downloads the proofs archive with in-process resume and size verification.
    ///
    /// `concurrency > 1` splits the archive into parallel Range streams. An
    /// existing sequential `.part` leftover is finished as a single stream so
    /// an interrupted one-stream download is not thrown away.
    async fn download_archive(
        entry: &ProofsManifestEntry,
        cache_dir: &Path,
        concurrency: usize,
    ) -> Result<std::path::PathBuf> {
        let dest_path = cache_dir.join(&entry.file_name);
        let part_path = cache_dir.join(format!("{}.part", entry.file_name));
        let sidecar_path = cache_dir.join(format!("{}.part.ranges", entry.file_name));
        let concurrency = concurrency.max(1);

        let existing_size = tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);
        let has_sidecar = tokio::fs::try_exists(&sidecar_path).await.unwrap_or(false);

        if existing_size == entry.expected_size && !has_sidecar {
            info!(target: "reth::cli", "Part file already matches expected size, skipping download");
            tokio::fs::rename(&part_path, &dest_path).await?;
            return Ok(dest_path);
        }

        if existing_size > entry.expected_size {
            info!(
                target: "reth::cli",
                existing_size,
                expected_size = entry.expected_size,
                "Part file exceeds expected size, restarting proofs download"
            );
            tokio::fs::remove_file(&part_path).await.ok();
            tokio::fs::remove_file(&sidecar_path).await.ok();
        }

        let sequential_leftover =
            existing_size > 0 && existing_size < entry.expected_size && !has_sidecar;

        if concurrency == 1 || sequential_leftover {
            if sequential_leftover && concurrency > 1 {
                info!(
                    target: "reth::cli",
                    existing_size,
                    "Finishing existing sequential proofs .part before using parallel ranges"
                );
            }
            return Self::download_archive_sequential(entry, cache_dir).await;
        }

        Self::download_archive_parallel(entry, &part_path, &dest_path, &sidecar_path, concurrency)
            .await
    }

    /// Downloads the proofs archive as a single resumable stream.
    ///
    /// Stream drops and truncated bodies retry from the current `.part` offset.
    /// A complete HTTP entity whose size does not match the manifest is still
    /// a hard error.
    async fn download_archive_sequential(
        entry: &ProofsManifestEntry,
        cache_dir: &Path,
    ) -> Result<std::path::PathBuf> {
        let dest_path = cache_dir.join(&entry.file_name);
        let part_path = cache_dir.join(format!("{}.part", entry.file_name));
        let sidecar_path = cache_dir.join(format!("{}.part.ranges", entry.file_name));

        if tokio::fs::try_exists(&sidecar_path).await.unwrap_or(false) {
            info!(
                target: "reth::cli",
                "Discarding parallel range sidecar and preallocated .part before sequential download"
            );
            tokio::fs::remove_file(&sidecar_path).await.ok();
            tokio::fs::remove_file(&part_path).await.ok();
        }

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(DOWNLOAD_READ_TIMEOUT)
            .build()?;

        info!(target: "reth::cli", url = %entry.archive_url, "Downloading proofs database");

        let mut idle_attempts = 0u32;
        loop {
            let mut existing_size =
                tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);

            if existing_size == entry.expected_size {
                info!(target: "reth::cli", "Part file already matches expected size, skipping download");
                tokio::fs::rename(&part_path, &dest_path).await?;
                return Ok(dest_path);
            }

            if existing_size > entry.expected_size {
                info!(
                    target: "reth::cli",
                    existing_size,
                    expected_size = entry.expected_size,
                    "Part file exceeds expected size, restarting proofs download"
                );
                tokio::fs::remove_file(&part_path).await.ok();
                existing_size = 0;
            }

            let mut request = client.get(&entry.archive_url);
            if existing_size > 0 {
                request = request.header("Range", format!("bytes={existing_size}-"));
                info!(target: "reth::cli", resume_from = existing_size, "Resuming proofs download");
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    Self::wait_before_retry(
                        &mut idle_attempts,
                        false,
                        false,
                        &format!("failed to download proofs from {}: {error}", entry.archive_url),
                    )
                    .await?;
                    continue;
                }
            };
            let status = response.status();

            if !status.is_success() {
                if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    Self::wait_before_retry(
                        &mut idle_attempts,
                        false,
                        true,
                        &format!(
                            "proofs download failed with HTTP {status}: {}",
                            entry.archive_url
                        ),
                    )
                    .await?;
                    continue;
                }
                eyre::bail!("proofs download failed with HTTP {status}: {}", entry.archive_url);
            }

            let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

            if existing_size > 0 && !is_resume {
                info!(target: "reth::cli", "Server returned full response despite range request, restarting download");
                tokio::fs::remove_file(&part_path).await.ok();
            }

            let start_size = if is_resume { existing_size } else { 0 };
            let content_length = response.content_length();

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(is_resume)
                .write(!is_resume)
                .truncate(!is_resume)
                .open(&part_path)
                .await?;

            let mut downloaded = start_size;
            let started = Instant::now();
            let mut last_log = started;
            let mut stream_error = None;

            let mut stream = response.bytes_stream();
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        file.write_all(&chunk).await?;
                        downloaded += chunk.len() as u64;
                        if last_log.elapsed() >= PROOFS_PROGRESS_LOG_INTERVAL {
                            log_proofs_download_progress(
                                downloaded,
                                entry.expected_size,
                                started,
                                start_size,
                            );
                            last_log = Instant::now();
                        }
                    }
                    Err(error) => {
                        stream_error = Some(error);
                        break;
                    }
                }
            }
            file.flush().await?;
            file.shutdown().await?;

            let downloaded_size = tokio::fs::metadata(&part_path).await?.len();
            let written_this_attempt = downloaded_size.saturating_sub(start_size);
            let entity_complete = stream_error.is_none()
                && content_length.map_or(downloaded_size == entry.expected_size, |len| {
                    written_this_attempt >= len
                });

            if !entity_complete {
                let reason = match (stream_error, content_length) {
                    (Some(error), _) => {
                        format!("stream interrupted downloading {}: {error}", entry.archive_url)
                    }
                    (None, Some(len)) => format!(
                        "truncated body downloading {}: received {written_this_attempt} of {len} bytes",
                        entry.archive_url
                    ),
                    (None, None) => format!(
                        "incomplete download without Content-Length {}: {downloaded_size}/{} bytes",
                        entry.archive_url, entry.expected_size
                    ),
                };
                Self::wait_before_retry(
                    &mut idle_attempts,
                    downloaded_size > start_size,
                    false,
                    &reason,
                )
                .await?;
                continue;
            }

            if downloaded_size != entry.expected_size {
                tokio::fs::remove_file(&part_path).await.ok();
                eyre::bail!(
                    "proofs archive size mismatch: downloaded {downloaded_size} bytes, \
                     manifest declares {} bytes — archive may be truncated or corrupt",
                    entry.expected_size
                );
            }

            tokio::fs::rename(&part_path, &dest_path).await?;
            return Ok(dest_path);
        }
    }

    /// Waits before retrying an interrupted proofs download.
    ///
    /// Progress resets the idle counter so a large Cloudflare GET can drop
    /// many times. Eight idle attempts without new bytes is treated as stalled.
    /// HTTP 429/5xx use exponential backoff; stream drops stay at 1s.
    async fn wait_before_retry(
        idle_attempts: &mut u32,
        made_progress: bool,
        throttled: bool,
        reason: &str,
    ) -> Result<()> {
        if made_progress {
            *idle_attempts = 0;
            debug!(target: "reth::cli", error = %reason, "Proofs download interrupted, resuming");
            return Ok(());
        }

        *idle_attempts += 1;
        if *idle_attempts >= MAX_IDLE_DOWNLOAD_ATTEMPTS {
            eyre::bail!(
                "proofs download stalled after {MAX_IDLE_DOWNLOAD_ATTEMPTS} attempts without progress: {reason}"
            );
        }

        let delay = retry_delay(*idle_attempts, throttled);
        debug!(
            target: "reth::cli",
            error = %reason,
            idle_attempts = *idle_attempts,
            retry_in_secs = delay.as_secs(),
            "Proofs download interrupted without progress, retrying"
        );
        tokio::time::sleep(delay).await;
        Ok(())
    }

    /// Downloads the proofs archive with parallel Range streams.
    async fn download_archive_parallel(
        entry: &ProofsManifestEntry,
        part_path: &Path,
        dest_path: &Path,
        sidecar_path: &Path,
        concurrency: usize,
    ) -> Result<std::path::PathBuf> {
        let ranges = split_byte_ranges(entry.expected_size, concurrency);
        let part_len = tokio::fs::metadata(part_path).await.map(|m| m.len()).unwrap_or(0);
        let written =
            load_trusted_range_sidecar(sidecar_path, concurrency, entry.expected_size, part_len)
                .unwrap_or_else(|| vec![0; ranges.len()]);
        let initial_progress: u64 = written.iter().sum();

        persist_range_sidecar(sidecar_path, concurrency, entry.expected_size, &written).await?;

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(part_path)
            .await?;
        file.set_len(entry.expected_size).await?;
        drop(file);

        info!(
            target: "reth::cli",
            url = %entry.archive_url,
            streams = ranges.len(),
            resume = %ProgressDisplay::bytes(initial_progress as f64),
            expected = %ProgressDisplay::bytes(entry.expected_size as f64),
            "Downloading proofs database with parallel Range requests"
        );

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(DOWNLOAD_READ_TIMEOUT)
            .pool_max_idle_per_host(concurrency)
            .build()?;
        let progress = Arc::new(AtomicU64::new(initial_progress));
        let written = Arc::new(Mutex::new(written));

        let ticker = {
            let progress = Arc::clone(&progress);
            let expected_size = entry.expected_size;
            let started = Instant::now();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(PROOFS_PROGRESS_LOG_INTERVAL);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    log_proofs_download_progress(
                        progress.load(Ordering::Relaxed),
                        expected_size,
                        started,
                        initial_progress,
                    );
                }
            })
        };

        let range_progress = RangeProgress {
            sidecar_path: sidecar_path.to_path_buf(),
            concurrency,
            expected_size: entry.expected_size,
            written,
            progress,
        };

        let downloads = ranges.iter().enumerate().map(|(range_idx, range)| {
            let client = client.clone();
            let url = entry.archive_url.clone();
            let part_path = part_path.to_path_buf();
            let range_progress = range_progress.clone();
            let range = range.clone();
            async move {
                Self::download_range(&client, &url, &part_path, range, range_idx, range_progress)
                    .await
            }
        });

        let result = try_join_all(downloads).await;
        ticker.abort();
        result?;

        let downloaded_size = tokio::fs::metadata(part_path).await?.len();
        if downloaded_size != entry.expected_size {
            tokio::fs::remove_file(part_path).await.ok();
            tokio::fs::remove_file(sidecar_path).await.ok();
            eyre::bail!(
                "proofs archive size mismatch: downloaded {downloaded_size} bytes, \
                 manifest declares {} bytes — archive may be truncated or corrupt",
                entry.expected_size
            );
        }

        tokio::fs::remove_file(sidecar_path).await.ok();
        tokio::fs::rename(part_path, dest_path).await?;
        Ok(dest_path.to_path_buf())
    }

    /// Downloads one byte range, retrying stream drops from the current offset.
    async fn download_range(
        client: &reqwest::Client,
        url: &str,
        part_path: &Path,
        range: std::ops::Range<u64>,
        range_idx: usize,
        range_progress: RangeProgress,
    ) -> Result<()> {
        let range_len = range.end.saturating_sub(range.start);
        let mut idle_attempts = 0u32;

        loop {
            let already = range_progress.written.lock().await[range_idx];
            if already >= range_len {
                return Ok(());
            }

            let abs_start = range.start + already;
            let response = match client
                .get(url)
                .header("Range", format!("bytes={abs_start}-{}", range.end.saturating_sub(1)))
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    Self::wait_before_retry(
                        &mut idle_attempts,
                        false,
                        false,
                        &format!(
                            "failed to download proofs range {abs_start}-{} from {url}: {error}",
                            range.end
                        ),
                    )
                    .await?;
                    continue;
                }
            };

            let status = response.status();
            if status != reqwest::StatusCode::PARTIAL_CONTENT {
                if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    Self::wait_before_retry(
                        &mut idle_attempts,
                        false,
                        true,
                        &format!("proofs range download failed with HTTP {status}: {url}"),
                    )
                    .await?;
                    continue;
                }
                eyre::bail!(
                    "expected HTTP 206 for proofs range {abs_start}-{}, got {status}: {url}",
                    range.end
                );
            }

            let content_length = response.content_length();
            let mut file = tokio::fs::OpenOptions::new().write(true).open(part_path).await?;
            file.seek(SeekFrom::Start(abs_start)).await?;

            let mut offset = abs_start;
            let mut stream_error = None;
            let mut stream = response.bytes_stream();
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        file.write_all(&chunk).await?;
                        offset += chunk.len() as u64;
                        range_progress.progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    }
                    Err(error) => {
                        stream_error = Some(error);
                        break;
                    }
                }
            }
            file.flush().await?;

            let done = offset.saturating_sub(range.start).min(range_len);
            {
                let mut written = range_progress.written.lock().await;
                written[range_idx] = done;
                persist_range_sidecar(
                    &range_progress.sidecar_path,
                    range_progress.concurrency,
                    range_progress.expected_size,
                    &written,
                )
                .await?;
            }

            if done >= range_len {
                return Ok(());
            }

            let written_this_attempt = offset.saturating_sub(abs_start);
            let truncated = content_length.is_some_and(|len| written_this_attempt < len);
            let reason = match stream_error {
                Some(error) => {
                    format!(
                        "stream interrupted downloading proofs range {abs_start}-{} from {url}: {error}",
                        range.end
                    )
                }
                None if truncated => {
                    format!(
                        "truncated proofs range {abs_start}-{} from {url}: received {written_this_attempt} of {} bytes",
                        range.end,
                        content_length.unwrap_or(0)
                    )
                }
                None => {
                    format!(
                        "incomplete proofs range {abs_start}-{} from {url}: {done}/{range_len} bytes",
                        range.end
                    )
                }
            };

            Self::wait_before_retry(&mut idle_attempts, written_this_attempt > 0, false, &reason)
                .await?;
        }
    }

    /// Extracts the archive and cleans up the cache directory.
    async fn extract_and_cleanup(
        archive_path: &Path,
        target_dir: &Path,
        cache_dir: &Path,
    ) -> Result<()> {
        info!(target: "reth::cli", "Extracting proofs archive");

        let extract_target = target_dir.to_path_buf();
        let extract_path = archive_path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::extract_tar_zst(&extract_path, &extract_target))
            .await??;

        tokio::fs::remove_file(archive_path).await.ok();
        tokio::fs::remove_dir_all(cache_dir).await.ok();

        info!(target: "reth::cli", "Proofs database download complete");
        Ok(())
    }

    /// Extracts a `.tar.zst` archive into the target directory.
    ///
    /// Framed archives are decompressed in parallel; single-frame archives published by older
    /// snapshotters are decompressed sequentially.
    fn extract_tar_zst(archive_path: &Path, target_dir: &Path) -> Result<()> {
        let file = std::fs::File::open(archive_path)
            .map_err(|e| eyre::eyre!("failed to open {}: {e}", archive_path.display()))?;
        let archive_size = file.metadata()?.len();
        let progress = ExtractionProgress::new(file, archive_size);
        let decoder = ZstdArchiveReader::new(BufReader::new(progress))?;
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(target_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        Router,
        body::{Body, Bytes},
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    use clap::Parser;
    use futures::stream;

    use super::*;
    use crate::chainspec::BaseChainSpecParser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: BaseDownloadCommand<BaseChainSpecParser>,
    }

    fn create_proofs_archive(content_pairs: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        let encoder = zstd::Encoder::new(&mut buf, 0).unwrap();
        let mut builder = tar::Builder::new(encoder);

        for (path, data) in content_pairs {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        buf
    }

    async fn start_test_server(
        manifest_json: serde_json::Value,
        archive_bytes: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let manifest_bytes = serde_json::to_vec(&manifest_json).unwrap();
        let manifest_clone = manifest_bytes.clone();
        let archive_clone = archive_bytes.clone();

        let app = Router::new()
            .route(
                "/manifest.json",
                get(move || {
                    let data = manifest_clone.clone();
                    async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], data) }
                }),
            )
            .route(
                "/proofs.tar.zst",
                get(move || {
                    let data = archive_clone.clone();
                    async move { data }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let manifest_url = format!("http://127.0.0.1:{}/manifest.json", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (manifest_url, handle)
    }

    async fn start_range_aware_server(
        archive_bytes: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app =
            Router::new().route("/proofs.tar.zst", get(handle_range)).with_state(archive_bytes);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (base_url, handle)
    }

    fn parse_byte_range(headers: &HeaderMap, len: usize) -> Option<(usize, usize)> {
        let spec = headers.get("Range")?.to_str().ok()?.strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        let start = start.parse::<usize>().ok()?;
        let end = if end.is_empty() { len.saturating_sub(1) } else { end.parse::<usize>().ok()? };
        let end = end.min(len.saturating_sub(1));
        (start < len && start <= end).then_some((start, end))
    }

    async fn handle_range(State(data): State<Vec<u8>>, headers: HeaderMap) -> impl IntoResponse {
        if let Some((start, end)) = parse_byte_range(&headers, data.len()) {
            return (
                StatusCode::PARTIAL_CONTENT,
                [(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", data.len()),
                )],
                data[start..=end].to_vec(),
            )
                .into_response();
        }
        (StatusCode::OK, data).into_response()
    }

    #[derive(Clone)]
    struct RecordingRangeState {
        data: Vec<u8>,
        requests: Arc<Mutex<Vec<(u64, u64)>>>,
    }

    /// Serves Range GETs and records each inclusive byte range that was asked for.
    async fn start_recording_range_server(
        archive_bytes: Vec<u8>,
    ) -> (String, Arc<Mutex<Vec<(u64, u64)>>>, tokio::task::JoinHandle<()>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().route("/proofs.tar.zst", get(handle_recording_range)).with_state(
            RecordingRangeState { data: archive_bytes, requests: Arc::clone(&requests) },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (base_url, requests, handle)
    }

    async fn handle_recording_range(
        State(state): State<RecordingRangeState>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        if let Some((start, end)) = parse_byte_range(&headers, state.data.len()) {
            state.requests.lock().await.push((start as u64, end as u64));
            return (
                StatusCode::PARTIAL_CONTENT,
                [(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", state.data.len()),
                )],
                state.data[start..=end].to_vec(),
            )
                .into_response();
        }

        state.requests.lock().await.push((0, state.data.len().saturating_sub(1) as u64));
        (StatusCode::OK, state.data).into_response()
    }

    #[derive(Clone)]
    struct DropThenRangeState {
        data: Vec<u8>,
        requests: Arc<AtomicUsize>,
    }

    async fn start_drop_then_range_server(
        archive_bytes: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/proofs.tar.zst", get(handle_drop_then_range)).with_state(
            DropThenRangeState { data: archive_bytes, requests: Arc::clone(&requests) },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (base_url, requests, handle)
    }

    async fn handle_drop_then_range(
        State(state): State<DropThenRangeState>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let request_n = state.requests.fetch_add(1, Ordering::SeqCst);
        let (start, end) = parse_byte_range(&headers, state.data.len())
            .unwrap_or_else(|| (0, state.data.len().saturating_sub(1)));

        if request_n == 0 {
            let remaining = end.saturating_sub(start) + 1;
            let drop_at = start + remaining / 2;
            let first = Bytes::from(state.data[start..drop_at].to_vec());
            let body = Body::from_stream(stream::iter([
                Ok::<_, std::io::Error>(first),
                Err(std::io::Error::other("error decoding response body")),
            ]));
            let status = if headers.get("Range").is_some() {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            };
            return (status, body).into_response();
        }

        (
            StatusCode::PARTIAL_CONTENT,
            [(
                axum::http::header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", state.data.len()),
            )],
            state.data[start..=end].to_vec(),
        )
            .into_response()
    }

    #[derive(Clone)]
    struct ConcurrentRangeState {
        data: Vec<u8>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    async fn start_concurrent_range_server(
        archive_bytes: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/proofs.tar.zst", get(handle_concurrent_range)).with_state(
            ConcurrentRangeState {
                data: archive_bytes,
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::clone(&max_in_flight),
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (base_url, max_in_flight, handle)
    }

    async fn handle_concurrent_range(
        State(state): State<ConcurrentRangeState>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let current = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.max_in_flight.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let response = handle_range(State(state.data), headers).await;
        state.in_flight.fetch_sub(1, Ordering::SeqCst);
        response
    }

    #[test]
    fn proofs_flag_is_parsed() {
        let cli = TestCli::parse_from(["test", "--proofs"]);
        assert!(cli.args.proofs, "--proofs should be true");
    }

    #[test]
    fn download_without_proofs_flag() {
        let cli = TestCli::parse_from(["test"]);
        assert!(!cli.args.proofs, "--proofs should default to false");
    }

    #[test]
    fn resolve_datadir_args_uses_explicit_datadir() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir"),
            OsString::from("/tmp/base-download-test"),
        ])
        .resolve_datadir(BaseChainSpec::mainnet().chain());

        assert_eq!(
            datadir.data_dir(),
            Path::new("/tmp/base-download-test"),
            "proofs download should use --datadir without adding the chain directory"
        );
    }

    #[test]
    fn resolve_datadir_args_uses_equals_syntax() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir=/tmp/base-download-test"),
        ])
        .resolve_datadir(BaseChainSpec::mainnet().chain());

        assert_eq!(
            datadir.data_dir(),
            Path::new("/tmp/base-download-test"),
            "proofs download should use --datadir=VALUE without adding the chain directory"
        );
    }

    #[test]
    fn resolve_manifest_url_arg_reads_separate_flag() {
        let url = resolve_manifest_url_arg([
            OsString::from("test"),
            OsString::from("--manifest-url"),
            OsString::from("https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"),
        ]);

        assert_eq!(
            url.as_deref(),
            Some("https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"),
            "proofs should reuse the same --manifest-url as reth's downloader"
        );
    }

    #[test]
    fn resolve_manifest_url_arg_reads_equals_syntax() {
        let url = resolve_manifest_url_arg([
            OsString::from("test"),
            OsString::from(
                "--manifest-url=https://zeronet-v2-snapshots.base.org/1789516802/manifest.json",
            ),
        ]);

        assert_eq!(
            url.as_deref(),
            Some("https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"),
            "proofs should reuse --manifest-url=VALUE"
        );
    }

    #[test]
    fn resolve_manifest_url_arg_is_none_without_flag() {
        let url = resolve_manifest_url_arg([
            OsString::from("test"),
            OsString::from("--proofs"),
            OsString::from("--chain"),
            OsString::from("base-zeronet"),
        ]);

        assert_eq!(url, None, "missing --manifest-url should fall back to snapshot API discovery");
    }

    #[test]
    fn resolve_download_concurrency_arg_defaults_to_reth() {
        let concurrency =
            resolve_download_concurrency_arg([OsString::from("test"), OsString::from("--proofs")]);

        assert_eq!(
            concurrency, DEFAULT_DOWNLOAD_CONCURRENCY,
            "proofs should use reth's default --download-concurrency"
        );
    }

    #[test]
    fn resolve_download_concurrency_arg_reads_separate_flag() {
        let concurrency = resolve_download_concurrency_arg([
            OsString::from("test"),
            OsString::from("--download-concurrency"),
            OsString::from("16"),
        ]);

        assert_eq!(concurrency, 16, "proofs should reuse --download-concurrency");
    }

    #[test]
    fn resolve_download_concurrency_arg_reads_equals_syntax() {
        let concurrency = resolve_download_concurrency_arg([
            OsString::from("test"),
            OsString::from("--download-concurrency=4"),
        ]);

        assert_eq!(concurrency, 4, "proofs should reuse --download-concurrency=VALUE");
    }

    #[test]
    fn resolve_download_concurrency_arg_rejects_zero() {
        let concurrency = resolve_download_concurrency_arg([
            OsString::from("test"),
            OsString::from("--download-concurrency"),
            OsString::from("0"),
        ]);

        assert_eq!(concurrency, 1, "zero concurrency should be clamped to one stream");
    }

    #[test]
    fn split_byte_ranges_covers_the_whole_file() {
        let ranges = split_byte_ranges(10, 4);

        assert_eq!(
            ranges,
            vec![0..3, 3..6, 6..8, 8..10],
            "remainder should land on the first ranges"
        );
        assert_eq!(ranges.last().map(|range| range.end), Some(10));
    }

    #[test]
    fn split_byte_ranges_clamps_parts_to_file_size() {
        let ranges = split_byte_ranges(3, 8);

        assert_eq!(ranges, vec![0..1, 1..2, 2..3], "cannot split into more streams than bytes");
    }

    #[test]
    fn retry_delay_keeps_stream_drops_at_one_second() {
        assert_eq!(retry_delay(1, false), std::time::Duration::from_secs(1));
        assert_eq!(retry_delay(8, false), std::time::Duration::from_secs(1));
    }

    #[test]
    fn retry_delay_uses_exponential_backoff_when_throttled() {
        assert_eq!(retry_delay(1, true), std::time::Duration::from_secs(2));
        assert_eq!(retry_delay(2, true), std::time::Duration::from_secs(4));
        assert_eq!(retry_delay(3, true), std::time::Duration::from_secs(8));
        assert_eq!(
            retry_delay(5, true),
            std::time::Duration::from_secs(30),
            "backoff should cap at 30s"
        );
    }

    async fn start_snapshot_api_server(
        listing: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listing_bytes = serde_json::to_vec(&listing).unwrap();
        let app = Router::new().route(
            "/api/snapshots",
            get(move || {
                let data = listing_bytes.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], data) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api_url = format!("http://127.0.0.1:{}/api/snapshots", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (api_url, handle)
    }

    #[tokio::test]
    async fn discover_latest_manifest_url_picks_latest_modular_for_chain() {
        let listing = serde_json::json!([
            {
                "chainId": "8453",
                "block": "100",
                "metadataUrl": "https://mainnet-v2-snapshots.base.org/old/manifest.json"
            },
            {
                "chainId": "763360",
                "block": "10",
                "metadataUrl": "https://zeronet-v2-snapshots.base.org/old/manifest.json"
            },
            {
                "chainId": "763360",
                "block": "20",
                "metadataUrl": "https://zeronet-v2-snapshots.base.org/new/manifest.json"
            },
            {
                "chainId": "763360",
                "block": "15",
                "metadataUrl": "https://zeronet-v2-snapshots.base.org/not-a-manifest.tar.zst"
            }
        ]);

        let (api_url, handle) = start_snapshot_api_server(listing).await;
        let url = discover_latest_manifest_url(&api_url, 763360).await.unwrap();

        assert_eq!(
            url, "https://zeronet-v2-snapshots.base.org/new/manifest.json",
            "proofs must use the latest modular metadataUrl for the requested chain"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn discover_latest_manifest_url_accepts_numeric_ids() {
        let listing = serde_json::json!([{
            "chainId": 763360,
            "block": 3584106,
            "metadataUrl": "https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"
        }]);

        let (api_url, handle) = start_snapshot_api_server(listing).await;
        let url = discover_latest_manifest_url(&api_url, 763360).await.unwrap();

        assert_eq!(
            url, "https://zeronet-v2-snapshots.base.org/1789516802/manifest.json",
            "snapshot API numeric chainId/block fields should be accepted"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn discover_latest_manifest_url_fails_when_chain_missing() {
        let listing = serde_json::json!([{
            "chainId": "8453",
            "block": "100",
            "metadataUrl": "https://mainnet-v2-snapshots.base.org/old/manifest.json"
        }]);

        let (api_url, handle) = start_snapshot_api_server(listing).await;
        let result = discover_latest_manifest_url(&api_url, 763360).await;

        assert!(result.is_err(), "missing chain should fail discovery");
        assert!(
            result.unwrap_err().to_string().contains("no modular snapshot manifest"),
            "error should name the missing modular snapshot"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_manifest_entry_extracts_proofs_metadata() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"data")]);
        let manifest = serde_json::json!({
            "block": 1000000,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": {
                    "file": "proofs.tar.zst",
                    "size": archive.len(),
                    "decompressed_size": 0,
                    "output_files": []
                }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, archive.clone()).await;
        let entry = ProofsDownloader::fetch_manifest_entry(&manifest_url).await.unwrap();

        assert_eq!(entry.file_name, "proofs.tar.zst");
        assert_eq!(entry.expected_size, archive.len() as u64);
        assert!(entry.archive_url.ends_with("/proofs.tar.zst"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_manifest_entry_rejects_path_traversal() {
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": {
                    "file": "../../etc/evil.tar.zst",
                    "size": 100,
                    "decompressed_size": 0,
                    "output_files": []
                }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, vec![]).await;
        let result = ProofsDownloader::fetch_manifest_entry(&manifest_url).await;

        assert!(result.is_err(), "path traversal should be rejected");
        assert!(result.unwrap_err().to_string().contains("invalid proofs file name"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_manifest_entry_fails_when_no_proofs() {
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "state": { "file": "state.tar.zst", "size": 100, "decompressed_size": 500, "output_files": [] }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, vec![]).await;
        let result = ProofsDownloader::fetch_manifest_entry(&manifest_url).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no 'proofs' component"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_manifest_entry_fails_when_size_missing() {
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": { "file": "proofs.tar.zst", "decompressed_size": 0, "output_files": [] }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, vec![]).await;
        let result = ProofsDownloader::fetch_manifest_entry(&manifest_url).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'size'"));

        handle.abort();
    }

    #[tokio::test]
    async fn full_pipeline_downloads_and_extracts() {
        let archive = create_proofs_archive(&[
            ("proofs/data.mdb", b"real-proof-data-from-server"),
            ("proofs/lock.mdb", b"lock-file"),
        ]);

        let manifest = serde_json::json!({
            "block": 1000000,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": {
                    "file": "proofs.tar.zst",
                    "size": archive.len(),
                    "decompressed_size": 0,
                    "output_files": []
                }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        ProofsDownloader::run_from_manifest(target.path(), &manifest_url, 1)
            .await
            .expect("full pipeline should succeed");

        assert_eq!(
            std::fs::read(target.path().join("proofs/data.mdb")).unwrap(),
            b"real-proof-data-from-server",
            "extracted content should match"
        );
        assert_eq!(std::fs::read(target.path().join("proofs/lock.mdb")).unwrap(), b"lock-file");
        assert!(!target.path().join(".snapshot-cache").exists(), "cache should be cleaned up");

        handle.abort();
    }

    #[test]
    fn extract_tar_zst_creates_files() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        let archive_path = src.path().join("proofs.tar.zst");
        std::fs::write(
            &archive_path,
            create_proofs_archive(&[("proofs/data.mdb", b"proof-data-contents")]),
        )
        .unwrap();

        ProofsDownloader::extract_tar_zst(&archive_path, dest.path()).unwrap();

        let extracted = dest.path().join("proofs/data.mdb");
        assert!(extracted.exists(), "extracted file should exist");
        assert_eq!(std::fs::read(&extracted).unwrap(), b"proof-data-contents");
    }

    #[test]
    fn extract_tar_zst_decodes_framed_archives() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        // Larger than one frame so extraction spans several independently compressed frames.
        let data: Vec<u8> = (0..base_reth_cli::FRAMED_ZSTD_CHUNK_SIZE * 2 + 17)
            .map(|index| (index % 251) as u8 ^ (index / 4096) as u8)
            .collect();

        let mut builder = tar::Builder::new(
            base_reth_cli::ZstdArchiveEncoder::new(
                Vec::new(),
                base_reth_cli::ArchiveCompression::Framed { workers: 3 },
            )
            .unwrap(),
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "proofs/000001.sst", data.as_slice()).unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        assert!(base_reth_cli::FramedZstdHeader::matches(&archive));

        let archive_path = src.path().join("proofs.tar.zst");
        std::fs::write(&archive_path, archive).unwrap();
        ProofsDownloader::extract_tar_zst(&archive_path, dest.path()).unwrap();

        assert_eq!(std::fs::read(dest.path().join("proofs/000001.sst")).unwrap(), data);
    }

    #[test]
    fn extract_tar_zst_preserves_directory_structure() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        let archive_path = src.path().join("proofs.tar.zst");
        std::fs::write(
            &archive_path,
            create_proofs_archive(&[
                ("proofs/data.mdb", b"data"),
                ("proofs/lock.mdb", b"lock"),
                ("proofs/nested/deep.dat", b"deep"),
            ]),
        )
        .unwrap();

        ProofsDownloader::extract_tar_zst(&archive_path, dest.path()).unwrap();

        assert!(dest.path().join("proofs/data.mdb").exists());
        assert!(dest.path().join("proofs/lock.mdb").exists());
        assert!(dest.path().join("proofs/nested/deep.dat").exists());
        assert_eq!(std::fs::read(dest.path().join("proofs/nested/deep.dat")).unwrap(), b"deep");
    }

    #[test]
    fn extract_tar_zst_fails_on_missing_archive() {
        let dest = tempfile::tempdir().unwrap();
        let result = ProofsDownloader::extract_tar_zst(
            &dest.path().join("nonexistent.tar.zst"),
            dest.path(),
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn download_archive_resumes_from_partial_file() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"complete-proof-data")]);
        let (base_url, handle) = start_range_aware_server(archive.clone()).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");

        let half = archive.len() / 2;
        std::fs::write(&part_path, &archive[..half]).unwrap();
        assert_eq!(
            std::fs::metadata(&part_path).unwrap().len(),
            half as u64,
            "part file should contain first half of archive"
        );

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 1).await.unwrap();
        let downloaded = std::fs::read(&dest).unwrap();

        assert_eq!(downloaded.len(), archive.len(), "resumed download should produce full archive");
        assert_eq!(downloaded, archive, "resumed archive should match original byte-for-byte");

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_retries_after_stream_interrupt() {
        let archive =
            create_proofs_archive(&[("proofs/data.mdb", b"complete-proof-data-after-cf-drop")]);
        let (base_url, requests, handle) = start_drop_then_range_server(archive.clone()).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 1)
            .await
            .expect("stream drop should resume in-process instead of failing");
        let downloaded = std::fs::read(&dest).unwrap();

        assert_eq!(
            downloaded, archive,
            "in-process resume after stream drop should yield the full archive"
        );
        assert!(
            requests.load(Ordering::SeqCst) >= 2,
            "stream drop should trigger a Range retry, got {} requests",
            requests.load(Ordering::SeqCst)
        );
        assert!(
            !cache_dir.path().join("proofs.tar.zst.part").exists(),
            "completed download should rename .part into place"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_retries_when_body_ends_without_content_length() {
        let archive =
            create_proofs_archive(&[("proofs/data.mdb", b"resume-after-silent-truncation")]);
        let requests = Arc::new(AtomicUsize::new(0));
        let app =
            Router::new().route("/proofs.tar.zst", get(handle_short_stream_then_range)).with_state(
                DropThenRangeState { data: archive.clone(), requests: Arc::clone(&requests) },
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let cache_dir = tempfile::tempdir().unwrap();
        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 1)
            .await
            .expect("a clean close without Content-Length should resume, not delete .part");

        assert_eq!(std::fs::read(&dest).unwrap(), archive);
        assert!(
            requests.load(Ordering::SeqCst) >= 2,
            "silent truncation should retry, got {} requests",
            requests.load(Ordering::SeqCst)
        );
        assert!(
            !cache_dir.path().join("proofs.tar.zst.part").exists(),
            "completed download should rename .part into place"
        );

        handle.abort();
    }

    async fn handle_short_stream_then_range(
        State(state): State<DropThenRangeState>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let request_n = state.requests.fetch_add(1, Ordering::SeqCst);
        if request_n == 0 {
            let half = state.data.len() / 2;
            let first = Bytes::from(state.data[..half].to_vec());
            let body = Body::from_stream(stream::iter([Ok::<_, std::io::Error>(first)]));
            return (StatusCode::OK, body).into_response();
        }

        handle_range(State(state.data), headers).await.into_response()
    }

    #[tokio::test]
    async fn download_archive_uses_parallel_range_requests() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"parallel-range-proof-data")]);
        let (base_url, max_in_flight, handle) =
            start_concurrent_range_server(archive.clone()).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 4)
            .await
            .expect("parallel Range download should succeed");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            archive,
            "assembled ranges should match the archive"
        );
        assert!(
            max_in_flight.load(Ordering::SeqCst) >= 2,
            "download-concurrency=4 should issue overlapping Range requests, max in-flight was {}",
            max_in_flight.load(Ordering::SeqCst)
        );
        assert!(
            !cache_dir.path().join("proofs.tar.zst.part.ranges").exists(),
            "range sidecar should be removed after a complete parallel download"
        );

        handle.abort();
    }

    fn write_test_sidecar(path: &Path, concurrency: usize, expected_size: u64, written: &[u64]) {
        let mut line = format!("{concurrency} {expected_size}");
        for amount in written {
            line.push(' ');
            line.push_str(&amount.to_string());
        }
        std::fs::write(path, line).unwrap();
    }

    #[test]
    fn load_trusted_range_sidecar_rejects_missing_or_short_part() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = dir.path().join("proofs.tar.zst.part.ranges");
        write_test_sidecar(&sidecar, 4, 100, &[25, 25, 25, 25]);

        assert_eq!(
            load_trusted_range_sidecar(&sidecar, 4, 100, 0),
            None,
            "sidecar without a full-sized .part must not skip ranges"
        );
        assert_eq!(
            load_trusted_range_sidecar(&sidecar, 4, 100, 50),
            None,
            "sidecar with a resized .part must not skip ranges"
        );
        assert_eq!(
            load_trusted_range_sidecar(&sidecar, 4, 100, 100),
            Some(vec![25, 25, 25, 25]),
            "sidecar should be trusted only when .part is the declared size"
        );
    }

    /// Verifies a crash-restart with a trusted sidecar only fetches unfinished ranges.
    #[tokio::test]
    async fn download_archive_parallel_resumes_from_trusted_sidecar() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"resume-two-of-four-ranges")]);
        let ranges = split_byte_ranges(archive.len() as u64, 4);
        assert_eq!(ranges.len(), 4, "archive must split into four ranges");

        let mut part = vec![0u8; archive.len()];
        for range in &ranges[..2] {
            let start = range.start as usize;
            let end = range.end as usize;
            part[start..end].copy_from_slice(&archive[start..end]);
        }

        let (base_url, requests, handle) = start_recording_range_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");
        std::fs::write(&part_path, &part).unwrap();
        write_test_sidecar(
            &cache_dir.path().join("proofs.tar.zst.part.ranges"),
            4,
            archive.len() as u64,
            &[ranges[0].end - ranges[0].start, ranges[1].end - ranges[1].start, 0, 0],
        );

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 4)
            .await
            .expect("trusted sidecar resume should finish the remaining ranges");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            archive,
            "resumed ranges plus already-written ranges should match the archive"
        );

        let mut got = requests.lock().await.clone();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![(ranges[2].start, ranges[2].end - 1), (ranges[3].start, ranges[3].end - 1),],
            "only the two incomplete ranges should be fetched"
        );

        handle.abort();
    }

    /// Verifies an in-flight sequential `.part` is finished as one stream, not `set_len`.
    #[tokio::test]
    async fn download_archive_finishes_sequential_leftover_before_parallel() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"finish-sequential-leftover")]);
        let half = archive.len() / 2;
        assert!(half > 0 && half < archive.len(), "archive must have a sequential midpoint");

        let (base_url, requests, handle) = start_recording_range_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_dir.path().join("proofs.tar.zst.part"), &archive[..half]).unwrap();

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 4)
            .await
            .expect("sequential leftover should finish as one stream");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            archive,
            "sequential leftover resume should yield the full archive"
        );
        assert_eq!(
            requests.lock().await.clone(),
            vec![(half as u64, archive.len() as u64 - 1)],
            "existing sequential .part must resume with one open-ended Range, not parallel splits"
        );
        assert!(
            !cache_dir.path().join("proofs.tar.zst.part.ranges").exists(),
            "sequential leftover must not write a parallel sidecar"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_sequential_discards_stale_parallel_part() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"not-zeros-from-set-len")]);
        let (base_url, handle) = start_range_aware_server(archive.clone()).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");
        std::fs::write(&part_path, vec![0u8; archive.len()]).unwrap();
        write_test_sidecar(
            &cache_dir.path().join("proofs.tar.zst.part.ranges"),
            4,
            archive.len() as u64,
            &[0, 0, 0, 0],
        );

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 1)
            .await
            .expect("sequential download should not accept a preallocated parallel .part");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            archive,
            "stale parallel .part filled with zeros must be re-downloaded"
        );
        assert!(
            !cache_dir.path().join("proofs.tar.zst.part.ranges").exists(),
            "stale sidecar should be removed"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_parallel_ignores_sidecar_without_part_file() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"download-over-missing-part")]);
        let (base_url, _max_in_flight, handle) =
            start_concurrent_range_server(archive.clone()).await;

        let cache_dir = tempfile::tempdir().unwrap();
        write_test_sidecar(
            &cache_dir.path().join("proofs.tar.zst.part.ranges"),
            4,
            archive.len() as u64,
            &[
                archive.len() as u64 / 4,
                archive.len() as u64 / 4,
                archive.len() as u64 / 4,
                archive.len() as u64 - 3 * (archive.len() as u64 / 4),
            ],
        );

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 4)
            .await
            .expect("missing .part should re-download instead of trusting the sidecar");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            archive,
            "sidecar without a matching .part must not leave zero-filled holes"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_parallel_retries_after_stream_interrupt() {
        let archive =
            create_proofs_archive(&[("proofs/data.mdb", b"parallel-proof-data-after-cf-drop")]);
        let (base_url, requests, handle) = start_drop_then_range_server(archive.clone()).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 4)
            .await
            .expect("parallel stream drop should resume the failed range in-process");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            archive,
            "parallel resume after stream drop should yield the full archive"
        );
        assert!(
            requests.load(Ordering::SeqCst) >= 2,
            "dropped range should retry, got {} requests",
            requests.load(Ordering::SeqCst)
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_restarts_when_server_ignores_range() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"fresh-data")]);

        let (base_url, handle) = {
            use axum::{Router, routing::get};

            let data = archive.clone();
            let app = Router::new().route(
                "/proofs.tar.zst",
                get(move || {
                    let d = data.clone();
                    async move { d }
                }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://127.0.0.1:{}", addr.port());
            let h = tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            (url, h)
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");
        std::fs::write(&part_path, b"stale-garbage-data-from-old-snapshot").unwrap();

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 1).await.unwrap();
        let downloaded = std::fs::read(&dest).unwrap();

        assert_eq!(downloaded, archive, "should discard stale .part and download fresh archive");

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_uses_completed_part_file_without_requesting_range() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"already-complete")]);

        let (base_url, handle) = {
            use axum::{Router, routing::get};

            let app = Router::new().route(
                "/proofs.tar.zst",
                get(|| async { (StatusCode::RANGE_NOT_SATISFIABLE, Vec::<u8>::new()) }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://127.0.0.1:{}", addr.port());
            let h = tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            (url, h)
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");
        std::fs::write(&part_path, &archive).unwrap();

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path(), 1).await.unwrap();

        assert_eq!(dest, cache_dir.path().join("proofs.tar.zst"));
        assert_eq!(std::fs::read(&dest).unwrap(), archive);
        assert!(!Path::new(&part_path).exists(), "completed .part should be renamed into place");

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_fails_on_size_mismatch() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"data")]);

        let (base_url, handle) = {
            use axum::{Router, routing::get};

            let data = archive.clone();
            let app = Router::new().route(
                "/proofs.tar.zst",
                get(move || {
                    let d = data.clone();
                    async move { d }
                }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://127.0.0.1:{}", addr.port());
            let h = tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            (url, h)
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64 + 999,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let result = ProofsDownloader::download_archive(&entry, cache_dir.path(), 1).await;

        assert!(result.is_err(), "size mismatch should fail");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("size mismatch"), "error should mention size mismatch, got: {err}");

        assert!(
            !cache_dir.path().join("proofs.tar.zst.part").exists(),
            "corrupt .part file should be deleted on size mismatch"
        );

        handle.abort();
    }
}
