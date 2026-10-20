//! Resumable download of the proofs archive as fixed-size pieces.
//!
//! Pieces are handed out in file order to `concurrency` workers so the
//! contiguous prefix of the `.part` file grows while the download runs and
//! extraction can follow it. Completed pieces are recorded in a sidecar next to
//! the `.part` file so an interrupted download resumes where it left off.
//!
//! Resume is best-effort across process interruption: pieces are flushed but
//! not fsynced, so after a machine crash the sidecar can mark bytes that never
//! reached disk. Extraction then fails, and the archive is downloaded again on
//! the next run.

use std::{
    fmt,
    io::{self, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    pin::pin,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use base_reth_cli::ProgressDisplay;
use eyre::Result;
use futures::{StreamExt, future::try_join_all};
use reqwest::{
    StatusCode,
    header::{CONTENT_RANGE, HeaderMap, RANGE},
};
use tokio::{
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::Mutex,
};
use tracing::{debug, info};

use super::{PROOFS_PROGRESS_LOG_INTERVAL, ProofsArchiveAvailability, ProofsManifestEntry};

/// Consecutive attempts at one piece that write no new bytes before failing.
///
/// Resets on progress so Cloudflare can drop a connection many times over a
/// multi-hundred-GiB download without aborting the process.
const MAX_IDLE_DOWNLOAD_ATTEMPTS: u32 = 8;

/// Idle read timeout so a stalled CDN socket fails into retry instead of hanging.
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Header of the piece bitmap sidecar format.
const PIECE_SIDECAR_VERSION: &str = "pieces-v1";

/// Which fixed-size pieces of the proofs archive are written to the `.part` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofsPieceMap {
    piece_size: u64,
    expected_size: u64,
    done: Vec<bool>,
}

impl ProofsPieceMap {
    /// Creates a map with no pieces written.
    pub fn new(piece_size: u64, expected_size: u64) -> Self {
        let piece_size = piece_size.max(1);
        let count = expected_size.div_ceil(piece_size) as usize;
        Self { piece_size, expected_size, done: vec![false; count] }
    }

    /// Creates a map with every piece written.
    pub fn complete(piece_size: u64, expected_size: u64) -> Self {
        let mut pieces = Self::new(piece_size, expected_size);
        pieces.done.fill(true);
        pieces
    }

    /// Byte range of piece `index`.
    pub fn range(&self, index: usize) -> Range<u64> {
        let start = index as u64 * self.piece_size;
        start..(start + self.piece_size).min(self.expected_size)
    }

    /// Records piece `index` as written.
    pub fn mark_done(&mut self, index: usize) {
        self.done[index] = true;
    }

    /// Length of the archive prefix made only of written pieces.
    pub fn contiguous_len(&self) -> u64 {
        self.done
            .iter()
            .position(|done| !done)
            .map_or(self.expected_size, |index| self.range(index).start)
    }

    /// Total bytes in written pieces.
    pub fn downloaded_len(&self) -> u64 {
        self.pieces(true).map(|(_, range)| range.end - range.start).sum()
    }

    /// Indices and byte ranges of pieces still to fetch, in file order.
    pub fn pending(&self) -> Vec<(usize, Range<u64>)> {
        self.pieces(false).collect()
    }

    /// Returns whether every piece is written.
    pub fn is_complete(&self) -> bool {
        self.done.iter().all(|done| *done)
    }

    /// Serializes the map as `pieces-v1 <piece_size> <expected_size> <bits>`.
    pub fn encode(&self) -> String {
        let bits: String = self.done.iter().map(|done| if *done { '1' } else { '0' }).collect();
        format!("{PIECE_SIDECAR_VERSION} {} {} {bits}", self.piece_size, self.expected_size)
    }

    /// Parses a sidecar written for the same piece size and archive size.
    ///
    /// Anything else, including sidecars from older downloader versions, is
    /// rejected so the download restarts instead of trusting unknown bytes.
    pub fn decode(text: &str, piece_size: u64, expected_size: u64) -> Option<Self> {
        let mut parts = text.split_whitespace();
        if parts.next()? != PIECE_SIDECAR_VERSION
            || parts.next()?.parse::<u64>().ok()? != piece_size
            || parts.next()?.parse::<u64>().ok()? != expected_size
        {
            return None;
        }

        let mut pieces = Self::new(piece_size, expected_size);
        let bits = parts.next().unwrap_or_default();
        if parts.next().is_some() || bits.len() != pieces.done.len() {
            return None;
        }
        for (done, bit) in pieces.done.iter_mut().zip(bits.chars()) {
            *done = match bit {
                '0' => false,
                '1' => true,
                _ => return None,
            };
        }
        Some(pieces)
    }

    fn pieces(&self, done: bool) -> impl Iterator<Item = (usize, Range<u64>)> + '_ {
        self.done
            .iter()
            .enumerate()
            .filter(move |(_, piece_done)| **piece_done == done)
            .map(|(index, _)| (index, self.range(index)))
    }
}

/// The server's archive size differs from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofsArchiveSizeMismatch {
    /// Size declared by the manifest.
    pub expected: u64,
    /// Size reported by the server's `Content-Range`.
    pub actual: u64,
}

impl fmt::Display for ProofsArchiveSizeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "proofs archive size mismatch: server reports {} bytes, manifest declares {} bytes",
            self.actual, self.expected
        )
    }
}

impl std::error::Error for ProofsArchiveSizeMismatch {}

/// One resumable piece-queue download of the proofs archive into its `.part` file.
#[derive(Debug)]
pub struct ProofsArchiveDownload {
    /// Archive URL. The server must answer Range requests with 206.
    pub url: String,
    /// File the archive is written to. It keeps this name until extraction
    /// finishes because the extractor opens it by path while it downloads.
    pub part_path: PathBuf,
    /// Resume state: the archive URL on the first line, then the encoded
    /// [`ProofsPieceMap`].
    pub sidecar_path: PathBuf,
    sidecar_tmp_path: PathBuf,
    expected_size: u64,
    piece_size: u64,
    concurrency: usize,
}

impl ProofsArchiveDownload {
    /// Piece size used for real downloads.
    pub const DEFAULT_PIECE_SIZE: u64 = 64 << 20;

    /// Describes the download of `entry` into `cache_dir`.
    pub fn new(
        entry: &ProofsManifestEntry,
        cache_dir: &Path,
        concurrency: usize,
        piece_size: u64,
    ) -> Self {
        Self {
            url: entry.archive_url.clone(),
            part_path: cache_dir.join(format!("{}.part", entry.file_name)),
            sidecar_path: cache_dir.join(format!("{}.part.ranges", entry.file_name)),
            sidecar_tmp_path: cache_dir.join(format!("{}.part.ranges.tmp", entry.file_name)),
            expected_size: entry.expected_size,
            piece_size: piece_size.max(1),
            concurrency: concurrency.max(1),
        }
    }

    /// Loads resume state and preallocates the `.part` file.
    ///
    /// A full-size `.part` without a sidecar is a finished download. A sidecar
    /// is trusted only when it names this archive URL and piece layout and the
    /// `.part` is full size. Anything else restarts from zero.
    pub async fn prepare(&self) -> Result<ProofsPieceMap> {
        let part_len = tokio::fs::metadata(&self.part_path).await.map(|m| m.len()).ok();
        let sidecar = match tokio::fs::read(&self.sidecar_path).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                eyre::bail!("failed to read {}: {error}", self.sidecar_path.display())
            }
        };

        let resumed = match (&sidecar, part_len) {
            (None, Some(len)) if len == self.expected_size => {
                info!(target: "reth::cli", "Proofs archive already downloaded, skipping download");
                return Ok(ProofsPieceMap::complete(self.piece_size, self.expected_size));
            }
            (Some(text), Some(len)) if len == self.expected_size => {
                text.split_once('\n').filter(|(url, _)| *url == self.url).and_then(|(_, map)| {
                    ProofsPieceMap::decode(map, self.piece_size, self.expected_size)
                })
            }
            _ => None,
        };

        let pieces = resumed.unwrap_or_else(|| {
            if sidecar.is_some() || part_len.is_some() {
                info!(
                    target: "reth::cli",
                    "Discarding incompatible partial proofs download, restarting from zero"
                );
            }
            ProofsPieceMap::new(self.piece_size, self.expected_size)
        });

        self.persist(&pieces).await?;
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.part_path)
            .await?;
        file.set_len(self.expected_size).await?;
        Ok(pieces)
    }

    /// Downloads every missing piece, publishing the contiguous prefix to
    /// `availability` as it grows.
    ///
    /// Removes the sidecar once complete. A size mismatch with the server
    /// deletes the `.part` file because none of it can be reused.
    pub async fn run(
        &self,
        pieces: ProofsPieceMap,
        availability: &ProofsArchiveAvailability,
    ) -> Result<()> {
        availability.publish(pieces.contiguous_len());
        if pieces.is_complete() {
            tokio::fs::remove_file(&self.sidecar_path).await.ok();
            return Ok(());
        }

        let pending = pieces.pending();
        let initial = pieces.downloaded_len();
        info!(
            target: "reth::cli",
            url = %self.url,
            streams = self.concurrency.min(pending.len()),
            resume = %ProgressDisplay::bytes(initial as f64),
            expected = %ProgressDisplay::bytes(self.expected_size as f64),
            "Downloading proofs archive"
        );

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(DOWNLOAD_READ_TIMEOUT)
            .pool_max_idle_per_host(self.concurrency)
            .build()?;
        let progress = AtomicU64::new(initial);
        let next = AtomicUsize::new(0);
        let pieces = Mutex::new(pieces);

        let workers = (0..self.concurrency.min(pending.len())).map(|_| async {
            while let Some((index, range)) = pending.get(next.fetch_add(1, Ordering::Relaxed)) {
                self.download_piece(&client, range.clone(), &progress).await?;
                let mut pieces = pieces.lock().await;
                pieces.mark_done(*index);
                self.persist(&pieces).await?;
                availability.publish(pieces.contiguous_len());
            }
            Ok::<_, eyre::Report>(())
        });
        let mut workers = pin!(try_join_all(workers));

        let started = Instant::now();
        let mut ticker = tokio::time::interval(PROOFS_PROGRESS_LOG_INTERVAL);
        ticker.tick().await;
        let result = loop {
            tokio::select! {
                result = &mut workers => break result,
                _ = ticker.tick() => Self::log_progress(
                    progress.load(Ordering::Relaxed),
                    self.expected_size,
                    started,
                    initial,
                ),
            }
        };

        if let Err(error) = result {
            if error.downcast_ref::<ProofsArchiveSizeMismatch>().is_some() {
                tokio::fs::remove_file(&self.part_path).await.ok();
                tokio::fs::remove_file(&self.sidecar_path).await.ok();
            }
            return Err(error);
        }

        tokio::fs::remove_file(&self.sidecar_path).await?;
        Ok(())
    }

    /// Atomically replaces the sidecar with `pieces`.
    async fn persist(&self, pieces: &ProofsPieceMap) -> Result<()> {
        tokio::fs::write(&self.sidecar_tmp_path, format!("{}\n{}", self.url, pieces.encode()))
            .await?;
        tokio::fs::rename(&self.sidecar_tmp_path, &self.sidecar_path).await?;
        Ok(())
    }

    /// Downloads one piece, retrying stream drops from the current offset.
    ///
    /// Returns after the piece is flushed to the `.part` file.
    async fn download_piece(
        &self,
        client: &reqwest::Client,
        range: Range<u64>,
        progress: &AtomicU64,
    ) -> Result<()> {
        let len = range.end - range.start;
        let last = range.end - 1;
        let url = &self.url;
        let mut written = 0u64;
        let mut idle_attempts = 0u32;

        while written < len {
            let start = range.start + written;
            let response =
                match client.get(url).header(RANGE, format!("bytes={start}-{last}")).send().await {
                    Ok(response) => response,
                    Err(error) => {
                        Self::wait_before_retry(
                            &mut idle_attempts,
                            false,
                            false,
                            &format!(
                                "failed to download proofs range {start}-{last} from {url}: {error}"
                            ),
                        )
                        .await?;
                        continue;
                    }
                };

            let status = response.status();
            if status != StatusCode::PARTIAL_CONTENT {
                if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
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
                    "expected HTTP 206 for proofs range {start}-{last}, got {status}: {url} \
                     (the server must support Range requests)"
                );
            }
            self.check_content_range(response.headers(), start, last)?;

            let content_length = response.content_length();
            let mut file = tokio::fs::OpenOptions::new().write(true).open(&self.part_path).await?;
            file.seek(SeekFrom::Start(start)).await?;

            let attempt_start = written;
            let mut stream_error = None;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        stream_error = Some(error);
                        break;
                    }
                };
                if chunk.len() as u64 > len - written {
                    eyre::bail!(
                        "server sent more than the requested bytes for proofs range {start}-{last}: {url}"
                    );
                }
                file.write_all(&chunk).await?;
                written += chunk.len() as u64;
                progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            file.flush().await?;

            if written >= len {
                break;
            }

            let received = written - attempt_start;
            let reason = match (stream_error, content_length) {
                (Some(error), _) => format!(
                    "stream interrupted downloading proofs range {start}-{last} from {url}: {error}"
                ),
                (None, Some(expected)) if received < expected => format!(
                    "truncated proofs range {start}-{last} from {url}: received {received} of {expected} bytes"
                ),
                (None, _) => format!(
                    "incomplete proofs range {start}-{last} from {url}: received {received} bytes"
                ),
            };
            Self::wait_before_retry(&mut idle_attempts, received > 0, false, &reason).await?;
        }

        Ok(())
    }

    /// Rejects a 206 response unless its `Content-Range` covers exactly
    /// `start..=last` of an archive with the size the manifest declares.
    fn check_content_range(&self, headers: &HeaderMap, start: u64, last: u64) -> Result<()> {
        let value = headers.get(CONTENT_RANGE).and_then(|value| value.to_str().ok());
        let parsed = value.and_then(|value| {
            let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
            let (first, end) = range.split_once('-')?;
            Some((first.parse::<u64>().ok()?, end.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
        });
        let Some((first, end, total)) = parsed else {
            eyre::bail!(
                "invalid Content-Range {value:?} for proofs range {start}-{last}: {}",
                self.url
            );
        };

        if total != self.expected_size {
            return Err(
                ProofsArchiveSizeMismatch { expected: self.expected_size, actual: total }.into()
            );
        }
        if (first, end) != (start, last) {
            eyre::bail!(
                "server returned proofs range {first}-{end}, requested {start}-{last}: {}",
                self.url
            );
        }
        Ok(())
    }

    /// Waits before retrying an interrupted piece.
    ///
    /// Progress resets the idle counter. [`MAX_IDLE_DOWNLOAD_ATTEMPTS`] idle
    /// attempts in a row is treated as stalled.
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

        let delay = Self::retry_delay(*idle_attempts, throttled);
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

    /// Delay before an idle retry.
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

    /// Logs download progress using the same fields as snapshot compression.
    fn log_progress(downloaded: u64, expected: u64, started: Instant, baseline: u64) {
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
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};

    use super::*;
    use crate::commands::download::proofs::test_utils::{
        create_proofs_archive, proofs_entry, serve, start_concurrent_range_server,
        start_drop_then_range_server, start_range_aware_server, start_recording_range_server,
        start_short_then_range_server,
    };

    /// Piece size that splits the small test archives into several pieces.
    const TEST_PIECE_SIZE: u64 = 16;

    /// Downloads `entry` into `cache_dir` and returns the `.part` path.
    async fn download(
        entry: &ProofsManifestEntry,
        cache_dir: &Path,
        concurrency: usize,
    ) -> Result<PathBuf> {
        let download = ProofsArchiveDownload::new(entry, cache_dir, concurrency, TEST_PIECE_SIZE);
        let pieces = download.prepare().await?;
        download.run(pieces, &ProofsArchiveAvailability::new(entry.expected_size)).await?;
        Ok(download.part_path)
    }

    #[test]
    fn retry_delay_keeps_stream_drops_at_one_second() {
        assert_eq!(ProofsArchiveDownload::retry_delay(1, false), Duration::from_secs(1));
        assert_eq!(ProofsArchiveDownload::retry_delay(8, false), Duration::from_secs(1));
    }

    #[test]
    fn retry_delay_uses_exponential_backoff_when_throttled() {
        assert_eq!(ProofsArchiveDownload::retry_delay(1, true), Duration::from_secs(2));
        assert_eq!(ProofsArchiveDownload::retry_delay(2, true), Duration::from_secs(4));
        assert_eq!(ProofsArchiveDownload::retry_delay(3, true), Duration::from_secs(8));
        assert_eq!(
            ProofsArchiveDownload::retry_delay(5, true),
            Duration::from_secs(30),
            "backoff should cap at 30s"
        );
    }

    #[test]
    fn piece_map_covers_the_whole_file() {
        let pieces = ProofsPieceMap::new(4, 10);

        assert_eq!(
            pieces.pending(),
            vec![(0, 0..4), (1, 4..8), (2, 8..10)],
            "the last piece should be short"
        );
    }

    #[test]
    fn piece_map_prefix_stops_at_first_missing_piece() {
        let mut pieces = ProofsPieceMap::new(4, 10);
        pieces.mark_done(1);
        pieces.mark_done(2);
        assert_eq!(pieces.contiguous_len(), 0, "a missing first piece hides later pieces");
        assert_eq!(pieces.downloaded_len(), 6);

        pieces.mark_done(0);
        assert_eq!(pieces.contiguous_len(), 10);
        assert!(pieces.is_complete());
    }

    #[test]
    fn piece_map_sidecar_round_trips() {
        let mut pieces = ProofsPieceMap::new(4, 10);
        pieces.mark_done(2);

        assert_eq!(ProofsPieceMap::decode(&pieces.encode(), 4, 10), Some(pieces));
    }

    #[test]
    fn piece_map_rejects_other_layouts_and_legacy_sidecars() {
        let pieces = ProofsPieceMap::new(4, 10).encode();

        assert_eq!(ProofsPieceMap::decode(&pieces, 8, 10), None, "different piece size");
        assert_eq!(ProofsPieceMap::decode(&pieces, 4, 11), None, "different archive size");
        assert_eq!(
            ProofsPieceMap::decode("4 10 3 3 2 2", 4, 10),
            None,
            "per-range byte counts from older versions"
        );
        assert_eq!(ProofsPieceMap::decode("pieces-v1 4 10 01", 4, 10), None, "short bitmap");
        assert_eq!(ProofsPieceMap::decode("pieces-v1 4 10 01x", 4, 10), None, "bad bit");
    }

    #[tokio::test]
    async fn download_retries_after_stream_interrupt() {
        let archive =
            create_proofs_archive(&[("proofs/data.mdb", b"complete-proof-data-after-cf-drop")]);
        let (base_url, requests, handle) = start_drop_then_range_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 1)
            .await
            .expect("stream drop should resume in-process instead of failing");

        assert_eq!(std::fs::read(&part).unwrap(), archive);
        assert!(
            requests.load(Ordering::SeqCst) >= 2,
            "stream drop should trigger a Range retry, got {} requests",
            requests.load(Ordering::SeqCst)
        );
        assert!(!cache_dir.path().join("proofs.tar.zst.part.ranges").exists());

        handle.abort();
    }

    #[tokio::test]
    async fn download_retries_when_body_ends_without_content_length() {
        let archive =
            create_proofs_archive(&[("proofs/data.mdb", b"resume-after-silent-truncation")]);
        let (base_url, requests, handle) = start_short_then_range_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 1)
            .await
            .expect("a clean close without Content-Length should resume");

        assert_eq!(std::fs::read(&part).unwrap(), archive);
        assert!(
            requests.load(Ordering::SeqCst) >= 2,
            "silent truncation should retry, got {} requests",
            requests.load(Ordering::SeqCst)
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_uses_parallel_range_requests() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"parallel-range-proof-data")]);
        let (base_url, max_in_flight, handle) =
            start_concurrent_range_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 4)
            .await
            .expect("parallel Range download should succeed");

        assert_eq!(std::fs::read(&part).unwrap(), archive, "pieces should assemble the archive");
        assert!(
            max_in_flight.load(Ordering::SeqCst) >= 2,
            "download-concurrency=4 should issue overlapping Range requests, max in-flight was {}",
            max_in_flight.load(Ordering::SeqCst)
        );
        assert!(
            !cache_dir.path().join("proofs.tar.zst.part.ranges").exists(),
            "sidecar should be removed after a complete download"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_resumes_only_missing_pieces_from_sidecar() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"resume-two-of-four-pieces")]);
        let mut pieces = ProofsPieceMap::new(TEST_PIECE_SIZE, archive.len() as u64);
        assert!(pieces.pending().len() >= 4, "archive must span at least four pieces");

        let mut part = vec![0u8; archive.len()];
        for index in [0, 2] {
            let range = pieces.range(index);
            let range = range.start as usize..range.end as usize;
            part[range.clone()].copy_from_slice(&archive[range]);
            pieces.mark_done(index);
        }

        let (base_url, requests, handle) = start_recording_range_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_dir.path().join("proofs.tar.zst.part"), &part).unwrap();
        std::fs::write(
            cache_dir.path().join("proofs.tar.zst.part.ranges"),
            format!("{base_url}/proofs.tar.zst\n{}", pieces.encode()),
        )
        .unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 4)
            .await
            .expect("sidecar resume should finish the remaining pieces");

        assert_eq!(std::fs::read(&part).unwrap(), archive);
        let mut got = requests.lock().await.clone();
        got.sort_unstable();
        let expected: Vec<_> =
            pieces.pending().into_iter().map(|(_, range)| (range.start, range.end - 1)).collect();
        assert_eq!(got, expected, "only pieces missing from the sidecar should be fetched");

        handle.abort();
    }

    #[tokio::test]
    async fn download_discards_partial_part_without_sidecar() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"discard-legacy-leftover")]);
        let (base_url, requests, handle) = start_recording_range_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        let mut stale = archive[..archive.len() / 2].to_vec();
        stale.fill(0xAA);
        std::fs::write(cache_dir.path().join("proofs.tar.zst.part"), &stale).unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 1)
            .await
            .expect("a legacy sequential leftover should restart the download");

        assert_eq!(std::fs::read(&part).unwrap(), archive);
        assert_eq!(
            requests.lock().await.first(),
            Some(&(0, TEST_PIECE_SIZE - 1)),
            "the download should restart from the first piece"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_discards_legacy_range_sidecar() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"not-zeros-from-set-len")]);
        let (base_url, handle) = start_range_aware_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        let len = archive.len() as u64;
        std::fs::write(cache_dir.path().join("proofs.tar.zst.part"), vec![0u8; archive.len()])
            .unwrap();
        std::fs::write(
            cache_dir.path().join("proofs.tar.zst.part.ranges"),
            format!("4 {len} {} {} {} {}", len / 4, len / 4, len / 4, len - 3 * (len / 4)),
        )
        .unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 4)
            .await
            .expect("a sidecar from an older version should restart the download");

        assert_eq!(
            std::fs::read(&part).unwrap(),
            archive,
            "a preallocated .part described by an old sidecar must be re-downloaded"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn download_ignores_sidecar_without_part_file() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"download-over-missing-part")]);
        let (base_url, handle) = start_range_aware_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            cache_dir.path().join("proofs.tar.zst.part.ranges"),
            format!(
                "{base_url}/proofs.tar.zst\n{}",
                ProofsPieceMap::complete(TEST_PIECE_SIZE, archive.len() as u64).encode()
            ),
        )
        .unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 4)
            .await
            .expect("missing .part should re-download instead of trusting the sidecar");

        assert_eq!(std::fs::read(&part).unwrap(), archive);

        handle.abort();
    }

    #[tokio::test]
    async fn download_discards_sidecar_for_another_archive_url() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"same-size-other-snapshot")]);
        let (base_url, handle) = start_range_aware_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_dir.path().join("proofs.tar.zst.part"), vec![0u8; archive.len()])
            .unwrap();
        std::fs::write(
            cache_dir.path().join("proofs.tar.zst.part.ranges"),
            format!(
                "https://example.com/old/proofs.tar.zst\n{}",
                ProofsPieceMap::complete(TEST_PIECE_SIZE, archive.len() as u64).encode()
            ),
        )
        .unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 4)
            .await
            .expect("resume state from another archive should restart the download");

        assert_eq!(std::fs::read(&part).unwrap(), archive);

        handle.abort();
    }

    #[tokio::test]
    async fn download_rejects_bad_content_range() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"content-range-checks")]);
        let len = archive.len();
        let cases = [
            (None, "invalid Content-Range"),
            (Some(format!("bytes 1-{TEST_PIECE_SIZE}/{len}")), "server returned proofs range"),
            (
                Some(format!("bytes 0-{}/{len}", TEST_PIECE_SIZE - 2)),
                "server returned proofs range",
            ),
            (Some(format!("bytes 0-{}/*", TEST_PIECE_SIZE - 1)), "invalid Content-Range"),
        ];

        for (content_range, expected_error) in cases {
            let body = archive[..TEST_PIECE_SIZE as usize].to_vec();
            let (base_url, handle) = serve(Router::new().route(
                "/proofs.tar.zst",
                get(move || {
                    let body = body.clone();
                    let content_range = content_range.clone();
                    async move {
                        let mut response = (StatusCode::PARTIAL_CONTENT, body).into_response();
                        if let Some(value) = content_range {
                            response.headers_mut().insert(CONTENT_RANGE, value.parse().unwrap());
                        }
                        response
                    }
                }),
            ))
            .await;
            let cache_dir = tempfile::tempdir().unwrap();

            let error = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 1)
                .await
                .expect_err("a response for other bytes must not fill a piece");

            assert!(error.to_string().contains(expected_error), "error: {error}");
            handle.abort();
        }
    }

    #[tokio::test]
    async fn download_fails_when_server_ignores_range() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"fresh-data")]);
        let data = archive.clone();
        let (base_url, handle) = serve(Router::new().route(
            "/proofs.tar.zst",
            get(move || {
                let data = data.clone();
                async move { data }
            }),
        ))
        .await;
        let cache_dir = tempfile::tempdir().unwrap();

        let error = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 1)
            .await
            .expect_err("a 200 response to a Range request cannot be placed in a piece");

        assert!(error.to_string().contains("expected HTTP 206"), "error: {error}");

        handle.abort();
    }

    #[tokio::test]
    async fn download_uses_completed_part_file_without_requests() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"already-complete")]);
        let (base_url, handle) = serve(Router::new().route(
            "/proofs.tar.zst",
            get(|| async { (StatusCode::RANGE_NOT_SATISFIABLE, Vec::<u8>::new()) }),
        ))
        .await;
        let cache_dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_dir.path().join("proofs.tar.zst.part"), &archive).unwrap();

        let part = download(&proofs_entry(&base_url, &archive), cache_dir.path(), 1)
            .await
            .expect("a full-size .part without a sidecar is a finished download");

        assert_eq!(std::fs::read(&part).unwrap(), archive);

        handle.abort();
    }

    #[tokio::test]
    async fn download_fails_on_size_mismatch() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"data")]);
        let (base_url, handle) = start_range_aware_server(archive.clone()).await;
        let cache_dir = tempfile::tempdir().unwrap();
        let mut entry = proofs_entry(&base_url, &archive);
        entry.expected_size += 999;

        let error = download(&entry, cache_dir.path(), 1)
            .await
            .expect_err("a server archive of a different size should fail");

        assert!(error.to_string().contains("size mismatch"), "error: {error}");
        assert!(
            !cache_dir.path().join("proofs.tar.zst.part").exists(),
            ".part should be deleted on size mismatch"
        );

        handle.abort();
    }
}
