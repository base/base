//! HTTP download with resume support and retry logic.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use backon::{ExponentialBuilder, Retryable};
use indicatif::ProgressBar;
use reqwest::StatusCode;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

/// Maximum number of retry attempts per archive download.
const MAX_RETRY_ATTEMPTS: usize = 5;

/// Minimum retry delay.
const RETRY_MIN_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Maximum retry delay.
const RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// HTTP request timeout for downloads.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Downloads archives from HTTP endpoints with resume support.
///
/// Uses `.part` files for partial downloads, enabling resumption after
/// interruptions. Supports HTTP Range requests when the server advertises
/// `Accept-Ranges: bytes`.
#[derive(Debug)]
pub struct ArchiveFetcher {
    client: reqwest::Client,
}

impl ArchiveFetcher {
    /// Creates a fetcher with a pre-configured HTTP client.
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { client })
    }

    /// Downloads a file from `url` to `dest_path` with resume support.
    ///
    /// If a `.part` file exists from a previous interrupted download, the
    /// fetcher sends a `Range` request to resume from the last byte. On
    /// success the `.part` file is renamed to `dest_path`.
    pub async fn download(
        &self,
        url: &str,
        dest_path: &Path,
        progress: Option<&ProgressBar>,
    ) -> Result<u64> {
        let part_path = dest_path.with_extension(
            dest_path
                .extension()
                .map(|e| format!("{}.part", e.to_string_lossy()))
                .unwrap_or_else(|| "part".to_string()),
        );

        let url_owned = url.to_string();
        let part_owned = part_path.clone();

        let total =
            (|| async { self.download_with_resume(&url_owned, &part_owned, progress).await })
                .retry(
                    ExponentialBuilder::default()
                        .with_min_delay(RETRY_MIN_DELAY)
                        .with_max_delay(RETRY_MAX_DELAY)
                        .with_max_times(MAX_RETRY_ATTEMPTS),
                )
                .when(|e| {
                    warn!(error = %e, "download failed, will retry");
                    true
                })
                .await
                .with_context(|| {
                    format!("failed to download {url} after {MAX_RETRY_ATTEMPTS} retries")
                })?;

        tokio::fs::rename(&part_path, dest_path).await.with_context(|| {
            format!("failed to rename {} to {}", part_path.display(), dest_path.display())
        })?;

        Ok(total)
    }

    /// Downloads a single archive, resuming from an existing `.part` file.
    async fn download_with_resume(
        &self,
        url: &str,
        part_path: &Path,
        progress: Option<&ProgressBar>,
    ) -> Result<u64> {
        let existing_size = tokio::fs::metadata(part_path).await.map(|m| m.len()).unwrap_or(0);

        let mut request = self.client.get(url);
        if existing_size > 0 {
            request = request.header("Range", format!("bytes={existing_size}-"));
            debug!(url = %url, resume_from = existing_size, "resuming download");
        }

        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        let status = response.status();

        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            debug!(url = %url, "range not satisfiable, restarting from scratch");
            tokio::fs::remove_file(part_path).await.ok();
            return Box::pin(self.download_with_resume(url, part_path, None)).await;
        }

        if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
            bail!("unexpected HTTP status {status} for {url}");
        }

        let is_resume = status == StatusCode::PARTIAL_CONTENT;
        let content_length = response.content_length().unwrap_or(0);
        let total_size = if is_resume { existing_size + content_length } else { content_length };

        if let Some(pb) = progress {
            pb.set_length(total_size);
            if is_resume {
                pb.set_position(existing_size);
            }
        }

        info!(
            url = %url,
            total_size,
            resumed = is_resume,
            "downloading archive"
        );

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(is_resume)
            .write(!is_resume)
            .truncate(!is_resume)
            .open(part_path)
            .await
            .with_context(|| format!("failed to open {}", part_path.display()))?;

        let mut stream = response.bytes_stream();
        use futures::StreamExt;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.with_context(|| format!("stream error downloading {url}"))?;
            file.write_all(&chunk).await?;
            if let Some(pb) = progress {
                pb.inc(chunk.len() as u64);
            }
        }

        file.flush().await?;

        let final_size = tokio::fs::metadata(part_path).await?.len();
        debug!(url = %url, size = final_size, "download complete");

        Ok(final_size)
    }

    /// Downloads a file and returns the path to the completed download.
    ///
    /// Convenience wrapper that builds the destination path from a URL and
    /// cache directory.
    pub async fn download_to_cache(
        &self,
        url: &str,
        cache_dir: &Path,
        progress: Option<&ProgressBar>,
    ) -> Result<PathBuf> {
        let file_name = url.rsplit('/').next().unwrap_or("archive.tar.zst");
        let dest = cache_dir.join(file_name);

        self.download(url, &dest, progress).await?;
        Ok(dest)
    }
}

impl Default for ArchiveFetcher {
    fn default() -> Self {
        Self::new().expect("failed to create HTTP client")
    }
}
