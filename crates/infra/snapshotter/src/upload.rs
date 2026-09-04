//! S3-compatible upload for snapshot artifacts with diff-based optimization.
//!
//! Artifacts are split into two areas within the bucket:
//!
//! - `{prefix}/static_files/` — static file chunks for finalized block ranges. The uploader
//!   compares the per-file BLAKE3 hashes recorded in the previous run's `manifest.json`
//!   against the freshly generated manifest, and skips chunks whose hashes match.
//!
//! - `{prefix}/{date}/` — per-run directory for mdbx state, rocksdb indices, proofs, the
//!   manifest, and the latest static-file chunk for every component. Keeping the mutable chunk
//!   beside the manifest prevents it from being overwritten while a downloader uses that manifest.

use std::{
    collections::HashMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use aws_sdk_s3::{
    Client as S3Client,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier},
};
use futures::stream::{self, StreamExt, TryStreamExt};
use tokio::{sync::mpsc, task::JoinHandle, time::sleep};
use tracing::{debug, error, info, warn};

use crate::{
    progress::{UploadProgress, UploadStage},
    snapshot::{ChunkFilename, ComponentManifest, SnapshotManifest, SnapshotManifestExt},
};

/// Maximum number of concurrent file uploads.
const MAX_CONCURRENT_UPLOADS: usize = 10;

/// Files larger than this threshold use multipart upload.
/// S3 `put_object` has a 5 `GiB` limit; we switch well below that.
const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Part size for multipart uploads (100 `MiB`).
const MULTIPART_PART_SIZE: u64 = 100 * 1024 * 1024;

/// Part size used for unknown-length streamed archives.
///
/// S3 limits a multipart object to 10,000 parts and 5 TiB. 640 MiB permits an archive as large
/// as the S3 object limit while leaving headroom below the part-count limit. This is deliberately
/// separate from the smaller file-backed upload part size: a file's total size is known before
/// upload, while a zstd stream's final compressed size is not.
const STREAMING_MULTIPART_PART_SIZE: usize = 640 * 1024 * 1024;

/// Number of complete multipart parts allowed to wait for upload per archive stream.
///
/// A stream producer also holds its current part while it is being filled. With the part size
/// above, a value of one bounds a single archive stream to roughly 1.25 GiB of compressed output
/// in memory (plus SDK request overhead), while still allowing the producer and uploader to run
/// concurrently.
const STREAMING_MULTIPART_CHANNEL_CAPACITY: usize = 1;

/// Base delay between upload retries. Backoff is linear to keep behavior simple and predictable.
const UPLOAD_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Cap for the retry backoff so uploads keep making progress instead of sleeping for minutes.
const MAX_UPLOAD_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Maximum number of objects per S3-compatible delete batch.
const DELETE_OBJECT_BATCH_SIZE: usize = 1000;

/// Completed timestamped snapshot run discovered from a published manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRun {
    /// Unix timestamp used as the run directory name.
    pub timestamp: u64,
    /// Full object key for the run's `manifest.json`.
    pub manifest_key: String,
}

/// Inputs for [`SnapshotUploader::upload`].
#[derive(Debug, Clone, Copy)]
pub struct SnapshotUploadParams<'a> {
    /// Directory containing the generated archives and `manifest.json`.
    pub output_dir: &'a Path,
    /// Generated artifact paths to upload.
    pub files: &'a [PathBuf],
    /// Unix timestamp used as the run directory name.
    pub timestamp: u64,
    /// Number of completed run directories to retain after upload.
    pub retain_runs: usize,
    /// Freshly generated local manifest.
    pub local_manifest: &'a SnapshotManifest,
    /// Previous run's published manifest, if any.
    pub remote_manifest: Option<&'a SnapshotManifest>,
    /// Shared `static_files/` listing from [`SnapshotUploader::list_remote_static_files`].
    pub remote_static_files: &'a HashMap<String, u64>,
}

/// Determines whether a snapshot component is re-uploaded every run
/// or can be skipped when the remote copy already matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStrategy {
    /// Always upload to the per-run date directory (mdbx, rocksdb, proofs, manifest).
    AlwaysUpload,
    /// Upload to `static_files/`, skipping if the per-file BLAKE3 hashes
    /// recorded in the previous run's manifest match the freshly generated ones.
    DiffByHash,
    /// Upload the latest static-file chunk to the timestamped run directory.
    LatestChunk,
}

impl UploadStrategy {
    /// Classifies a snapshot filename into its upload strategy.
    ///
    /// Static file chunks follow the pattern `{component}-{start}-{end}.tar.zst`
    /// (e.g. `headers-0-499999.tar.zst`). Finalized chunks are deduplicated in
    /// `static_files/`; the latest chunk is kept beside the timestamped manifest.
    ///
    /// Everything else (state, rocksdb, proofs, manifest) is always uploaded.
    pub fn classify(filename: &str) -> Self {
        if ChunkFilename::parse(filename).is_some() { Self::DiffByHash } else { Self::AlwaysUpload }
    }

    /// Classifies a snapshot filename using its manifest to identify the latest chunk.
    pub fn classify_with_manifest(filename: &str, manifest: &SnapshotManifest) -> Self {
        if manifest.is_latest_chunk_file(filename) {
            Self::LatestChunk
        } else {
            Self::classify(filename)
        }
    }
}

/// Uploads snapshot artifacts to an S3-compatible store (R2, `MinIO`, etc.).
#[derive(Debug, Clone)]
pub struct SnapshotUploader {
    client: S3Client,
    bucket: String,
    prefix: String,
    public_base_url: Option<String>,
}

/// A synchronous [`io::Write`] sink backed by an asynchronous S3 multipart upload.
///
/// Snapshot archive generation is synchronous (tar and zstd write through `io::Write`), whereas
/// the AWS SDK is asynchronous. This type bridges those models with a bounded channel: complete
/// 640 MiB parts are handed to an async task, and the synchronous producer blocks when that task
/// has not consumed the previous part. Consequently, archive bytes are never staged in a local
/// file and memory remains bounded per active archive.
///
/// Call [`Self::finish`] only after the archive writer has been finalized (for zstd, after
/// `Encoder::finish`). Then call [`Self::complete`] to wait for S3 to complete the multipart
/// upload. Dropping this value without finishing deliberately aborts the multipart upload rather
/// than publishing a truncated archive.
#[derive(Debug)]
pub struct StreamingMultipartUpload {
    key: String,
    sender: Option<mpsc::Sender<StreamingUploadMessage>>,
    buffered: Vec<u8>,
    bytes_written: u64,
    finished: bool,
    task: JoinHandle<Result<u64>>,
}

#[derive(Debug)]
enum StreamingUploadMessage {
    Part(Vec<u8>),
    Finish,
}

impl StreamingMultipartUpload {
    /// Returns the destination object key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the number of compressed bytes accepted from the archive writer so far.
    pub const fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Flushes the final (possibly smaller than 5 MiB) multipart part and marks the archive input
    /// complete. The final part is legal because all preceding parts are exactly 640 MiB.
    ///
    /// This does not wait for the remote object to become visible; use [`Self::complete`] for
    /// that. It is idempotent so cleanup paths can safely call it after a successful finish.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }

        if !self.buffered.is_empty() {
            let final_part = std::mem::take(&mut self.buffered);
            self.send_part(final_part)?;
        }

        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "streaming upload is closed"))?
            .blocking_send(StreamingUploadMessage::Finish)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "streaming multipart uploader stopped before archive finalization",
                )
            })?;
        self.sender.take();
        self.finished = true;
        Ok(())
    }

    /// Waits for all buffered parts to upload and for S3 to commit the multipart object.
    ///
    /// The archive must have been finalized first with [`Self::finish`]. If generation fails,
    /// drop the value instead; its background task aborts the incomplete multipart upload.
    pub async fn complete(self) -> Result<u64> {
        if !self.finished {
            bail!(
                "streaming multipart upload for {} was not finalized; call finish after the zstd encoder finishes",
                self.key
            );
        }
        self.task.await.context("streaming multipart upload task panicked")?
    }

    fn send_part(&mut self, part: Vec<u8>) -> io::Result<()> {
        debug_assert!(!part.is_empty());
        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "streaming upload is closed"))?
            .blocking_send(StreamingUploadMessage::Part(part))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "streaming multipart uploader stopped while writing archive",
                )
            })
    }
}

impl io::Write for StreamingMultipartUpload {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "cannot write after streaming multipart upload has finished",
            ));
        }

        let mut remaining = input;
        while !remaining.is_empty() {
            let available = STREAMING_MULTIPART_PART_SIZE - self.buffered.len();
            let take = available.min(remaining.len());
            self.buffered.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];

            if self.buffered.len() == STREAMING_MULTIPART_PART_SIZE {
                let full_part = std::mem::replace(
                    &mut self.buffered,
                    Vec::with_capacity(STREAMING_MULTIPART_PART_SIZE),
                );
                self.send_part(full_part)?;
            }
        }

        self.bytes_written = self.bytes_written.saturating_add(input.len() as u64);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // zstd may call flush while it is still writing its frame. A multipart part smaller than
        // 5 MiB is only valid as the final part, so only `finish` may flush `buffered`.
        Ok(())
    }
}

impl SnapshotUploader {
    /// Creates a new uploader.
    pub const fn new(
        client: S3Client,
        bucket: String,
        prefix: String,
        public_base_url: Option<String>,
    ) -> Self {
        Self { client, bucket, prefix, public_base_url }
    }

    /// Starts a multipart upload that accepts archive bytes through a synchronous
    /// [`io::Write`] implementation.
    ///
    /// This is intended for archive generators such as Reth's snapshot-manifest command, which
    /// synchronously write a tar/zstd stream. The caller must invoke
    /// [`StreamingMultipartUpload::finish`] *after* the zstd encoder has emitted its frame
    /// trailer, then await [`StreamingMultipartUpload::complete`] before publishing a manifest.
    ///
    /// The key is deliberately a complete object key rather than a filename. The caller owns the
    /// upload-plan decision (timestamped run directory versus immutable `static_files/` object)
    /// before archive generation begins.
    pub async fn start_streaming_multipart_upload(
        &self,
        key: impl Into<String>,
    ) -> Result<StreamingMultipartUpload> {
        let key = key.into();
        let upload_id = self.create_streaming_multipart_upload(&key).await?;
        let (sender, receiver) = mpsc::channel(STREAMING_MULTIPART_CHANNEL_CAPACITY);
        let task_uploader = self.clone();
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            task_uploader.consume_streaming_multipart_upload(task_key, upload_id, receiver).await
        });

        debug!(key = %key, "started streaming multipart upload");
        Ok(StreamingMultipartUpload {
            key,
            sender: Some(sender),
            buffered: Vec::new(),
            bytes_written: 0,
            finished: false,
            task,
        })
    }

    async fn create_streaming_multipart_upload(&self, key: &str) -> Result<String> {
        let response = retry_upload(
            || async {
                self.client
                    .create_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|error| UploadAttemptError::retry(error.into()))
            },
            |attempt, error| {
                warn!(
                    key = %key,
                    attempt,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "streaming multipart upload creation failed, retrying"
                );
            },
            |attempt| {
                info!(key = %key, attempt, "streaming multipart upload creation succeeded after retrying");
            },
        )
        .await?;

        response
            .upload_id()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("no upload_id returned for streaming upload {key}"))
    }

    /// Receives fully-owned multipart buffers and publishes the object only after receiving the
    /// explicit `Finish` signal. A closed channel is an aborted archive generation, not a valid
    /// end-of-file: that distinction prevents a tar/zstd stream that failed before `finish` from
    /// being exposed as a completed S3 object.
    async fn consume_streaming_multipart_upload(
        &self,
        key: String,
        upload_id: String,
        mut receiver: mpsc::Receiver<StreamingUploadMessage>,
    ) -> Result<u64> {
        let result = async {
            let mut completed_parts = Vec::new();
            let mut bytes_uploaded = 0u64;
            let mut part_number = 1i32;
            let mut received_finish = false;

            while let Some(message) = receiver.recv().await {
                match message {
                    StreamingUploadMessage::Part(part) => {
                        if part_number > 10_000 {
                            bail!("streaming multipart upload for {key} exceeds S3's 10,000-part limit");
                        }
                        let part_len = u64::try_from(part.len())?;
                        let completed = self
                            .upload_streaming_part(&key, &upload_id, part_number, part)
                            .await?;
                        completed_parts.push(completed);
                        bytes_uploaded = bytes_uploaded.saturating_add(part_len);
                        part_number += 1;
                    }
                    StreamingUploadMessage::Finish => {
                        received_finish = true;
                        break;
                    }
                }
            }

            if !received_finish {
                bail!("streaming archive writer for {key} was dropped before it finalized the zstd stream");
            }
            if completed_parts.is_empty() {
                bail!("streaming multipart upload for {key} contained no archive bytes");
            }

            self.complete_streaming_multipart_upload(
                &key,
                &upload_id,
                bytes_uploaded,
                completed_parts,
            )
            .await?;
            Ok(bytes_uploaded)
        }
        .await;

        match result {
            Ok(bytes_uploaded) => Ok(bytes_uploaded),
            Err(error) => {
                self.abort_streaming_multipart_upload(&key, &upload_id).await;
                Err(error)
            }
        }
    }

    async fn upload_streaming_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        bytes: Vec<u8>,
    ) -> Result<CompletedPart> {
        let length = bytes.len();
        retry_upload(
            || async {
                let upload_resp = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .body(ByteStream::from(bytes.clone()))
                    .send()
                    .await
                    .map_err(|error| UploadAttemptError::retry(error.into()))?;

                let e_tag = upload_resp
                    .e_tag()
                    .ok_or_else(|| anyhow::anyhow!("no ETag for streamed part {part_number} of {key}"))
                    .map_err(UploadAttemptError::fatal)?
                    .to_owned();
                Ok(CompletedPart::builder().part_number(part_number).e_tag(e_tag).build())
            },
            |attempt, error| {
                warn!(
                    key = %key,
                    upload_id,
                    part_number,
                    length,
                    attempt,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "streaming multipart upload part failed, retrying"
                );
            },
            |attempt| {
                info!(key = %key, upload_id, part_number, attempt, "streaming multipart upload part succeeded after retrying");
            },
        )
        .await
    }

    async fn complete_streaming_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
        size: u64,
        parts: Vec<CompletedPart>,
    ) -> Result<()> {
        retry_upload(
            || async {
                let completed = CompletedMultipartUpload::builder().set_parts(Some(parts.clone())).build();
                match self
                    .client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(error) => {
                        let error = anyhow::Error::from(error);
                        if self.remote_object_size(key).await.map_err(UploadAttemptError::fatal)?
                            == Some(size)
                        {
                            warn!(
                                key = %key,
                                upload_id,
                                error = %error,
                                error_debug = ?error,
                                size,
                                "streaming multipart completion response failed after object appeared; treating upload as successful"
                            );
                            Ok(())
                        } else {
                            Err(UploadAttemptError::retry(error))
                        }
                    }
                }
            },
            |attempt, error| {
                warn!(
                    key = %key,
                    upload_id,
                    attempt,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "streaming multipart upload completion failed, retrying"
                );
            },
            |attempt| {
                info!(key = %key, upload_id, attempt, "streaming multipart upload completion succeeded after retrying");
            },
        )
        .await
    }

    async fn abort_streaming_multipart_upload(&self, key: &str, upload_id: &str) {
        match self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
        {
            Ok(_) => {
                warn!(key = %key, upload_id, "aborted streaming multipart upload after failure")
            }
            Err(error) => error!(
                key = %key,
                upload_id,
                error = %error,
                error_debug = ?error,
                "failed to abort streaming multipart upload after failure"
            ),
        }
    }

    /// Lists remote static files with their sizes. Call once and pass the result
    /// to `generate_manifest` for skip-range computation.
    pub async fn list_remote_static_files(&self) -> Result<HashMap<String, u64>> {
        self.list_remote_objects(&self.static_files_prefix()).await
    }

    /// Fetches the most recent `manifest.json` from a prior run, if one exists.
    ///
    /// Looks for keys matching `{prefix}/{digits}/manifest.json` (where `{digits}`
    /// is the run's unix-timestamp directory) and downloads the one with the
    /// largest timestamp. Returns `None` on a fresh bucket. A parse error on the
    /// found manifest is logged and treated as no-previous (so we fall back to
    /// re-uploading everything rather than failing the run).
    pub async fn fetch_previous_manifest(&self) -> Result<Option<SnapshotManifest>> {
        let runs = self.list_completed_runs().await?;
        let best = runs.into_iter().max_by_key(|run| run.timestamp);

        let Some(run) = best else {
            debug!("no previous manifest found");
            return Ok(None);
        };

        debug!(timestamp = run.timestamp, key = %run.manifest_key, "fetching previous manifest");
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&run.manifest_key)
            .send()
            .await
            .with_context(|| format!("failed to fetch previous manifest {}", run.manifest_key))?;

        let bytes = resp
            .body
            .collect()
            .await
            .with_context(|| format!("failed to read previous manifest body {}", run.manifest_key))?
            .into_bytes();

        match serde_json::from_slice::<SnapshotManifest>(&bytes) {
            Ok(manifest) => Ok(Some(manifest)),
            Err(e) => {
                warn!(error = %e, key = %run.manifest_key, "failed to parse previous manifest, treating as missing");
                Ok(None)
            }
        }
    }

    /// Lists completed timestamped run directories by looking for published manifests.
    pub async fn list_completed_runs(&self) -> Result<Vec<SnapshotRun>> {
        let list_prefix =
            if self.prefix.is_empty() { String::new() } else { format!("{}/", self.prefix) };

        let mut run_prefixes = Vec::new();
        let mut continuation_token = None;

        loop {
            let mut req = self.client.list_objects_v2().bucket(&self.bucket).delimiter("/");
            if !list_prefix.is_empty() {
                req = req.prefix(&list_prefix);
            }
            if let Some(token) = continuation_token.take() {
                req = req.continuation_token(token);
            }

            let resp = req
                .send()
                .await
                .with_context(|| format!("failed to list objects under {list_prefix}"))?;

            for common_prefix in resp.common_prefixes() {
                let Some(prefix) = common_prefix.prefix() else { continue };
                if let Some((timestamp, run_prefix)) = Self::parse_run_prefix(prefix, &list_prefix)
                {
                    run_prefixes.push((timestamp, run_prefix));
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(String::from);
            } else {
                break;
            }
        }

        run_prefixes.sort_unstable_by_key(|run| std::cmp::Reverse(run.0));

        let mut runs = Vec::new();
        for (timestamp, run_prefix) in run_prefixes {
            let manifest_key = format!("{run_prefix}/manifest.json");
            if self.remote_key_exists(&manifest_key).await? {
                runs.push(SnapshotRun { timestamp, manifest_key });
            }
        }

        Ok(runs)
    }

    /// Prunes old completed timestamped run directories, retaining the newest `retain_runs`.
    pub async fn prune_old_runs(&self, retain_runs: usize) -> Result<()> {
        let runs = self.list_completed_runs().await?;
        let expired = Self::expired_run_timestamps(&runs, retain_runs);

        if expired.is_empty() {
            info!(retain_runs, total_runs = runs.len(), "no old snapshot runs to prune");
            return Ok(());
        }

        let mut last_error = None;
        for timestamp in expired {
            let run_prefix = self.run_prefix(timestamp);
            match self.delete_prefix(&run_prefix).await {
                Ok(deleted_objects) => {
                    info!(timestamp, run_prefix = %run_prefix, deleted_objects, "pruned old snapshot run");
                }
                Err(e) => {
                    warn!(error = %e, timestamp, run_prefix = %run_prefix, "failed to prune old snapshot run, continuing");
                    last_error = Some(e);
                }
            }
        }

        if let Some(e) = last_error {
            return Err(e);
        }

        Ok(())
    }

    /// Parses a common prefix of the form `{list_prefix}{timestamp}/`.
    pub fn parse_run_prefix(prefix: &str, list_prefix: &str) -> Option<(u64, String)> {
        let rest = prefix.strip_prefix(list_prefix)?;
        let timestamp_str = rest.strip_suffix('/')?;
        if timestamp_str.contains('/') {
            return None;
        }
        let timestamp = timestamp_str.parse::<u64>().ok()?;
        Some((timestamp, prefix.trim_end_matches('/').to_string()))
    }

    /// Returns the completed run timestamps that exceed the retention window.
    pub fn expired_run_timestamps(runs: &[SnapshotRun], retain_runs: usize) -> Vec<u64> {
        let mut timestamps: Vec<u64> = runs.iter().map(|run| run.timestamp).collect();
        timestamps.sort_unstable_by(|a, b| b.cmp(a));
        timestamps.dedup();
        timestamps.into_iter().skip(retain_runs).collect()
    }

    /// Uploads snapshot artifacts with diff-based optimization.
    ///
    /// Finalized static file chunks go to `{prefix}/static_files/` and are skipped
    /// only when their per-file BLAKE3 hashes (recorded in `local_manifest`) match
    /// `remote_manifest` and the archive exists in shared storage. The latest chunk
    /// of each component, state, rocksdb, proofs, and manifest go to
    /// `{prefix}/{timestamp}/`. `manifest.json` is uploaded last as the "snapshot complete"
    /// signal.
    ///
    /// `remote_static_files` is the shared-storage listing from
    /// [`Self::list_remote_static_files`]; callers that already listed for generation
    /// should reuse that map instead of listing again.
    pub async fn upload(&self, params: SnapshotUploadParams<'_>) -> Result<String> {
        let static_prefix = self.static_files_prefix();
        let run_prefix = self.run_prefix(params.timestamp);

        info!(
            run_prefix = %run_prefix,
            static_prefix = %static_prefix,
            file_count = params.files.len(),
            bucket = %self.bucket,
            "uploading snapshot artifacts"
        );

        let manifest_path = params.output_dir.join("manifest.json");
        let mut static_uploads = Vec::new();
        let mut run_uploads = Vec::new();
        let mut skipped = 0u64;

        for file in params.files {
            if file == &manifest_path {
                continue;
            }

            let file_name = file
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("invalid file path: {}", file.display()))?
                .to_string_lossy()
                .to_string();

            let strategy =
                UploadStrategy::classify_with_manifest(&file_name, params.local_manifest);

            match strategy {
                UploadStrategy::DiffByHash => {
                    let local_hashes = params.local_manifest.chunk_hashes_for_file(&file_name);
                    let remote_hashes =
                        params.remote_manifest.and_then(|m| m.chunk_hashes_for_file(&file_name));
                    match (&local_hashes, &remote_hashes) {
                        (Some(local), Some(remote))
                            if local == remote
                                && params.remote_static_files.contains_key(&file_name) =>
                        {
                            debug!(file = %file_name, "skipping finalized static file (blake3 matches shared object)");
                            skipped += 1;
                            continue;
                        }
                        (Some(local), Some(remote)) if local == remote => {
                            debug!(file = %file_name, "uploading finalized static file missing from shared storage");
                        }
                        (Some(_), Some(_)) => {
                            debug!(file = %file_name, "re-uploading static file (blake3 mismatch)");
                        }
                        _ => {
                            debug!(file = %file_name, "re-uploading static file (no prior hash available)");
                        }
                    }
                    static_uploads.push(file.clone());
                }
                UploadStrategy::LatestChunk | UploadStrategy::AlwaysUpload => {
                    run_uploads.push(file.clone());
                }
            }
        }

        info!(
            static_uploads = static_uploads.len(),
            run_uploads = run_uploads.len(),
            skipped,
            "diff analysis complete"
        );

        let static_upload_count = static_uploads.len();
        let run_upload_count = run_uploads.len();

        let progress = UploadProgress::new(&static_uploads, &run_uploads, &manifest_path).await?;
        let progress_logger = progress.spawn_logger();

        let manifest_key = format!("{run_prefix}/manifest.json");
        let upload_result = async {
            let static_prefix_ref = &static_prefix;
            let progress_ref = &progress;
            stream::iter(static_uploads)
                .map(|file| async move {
                    self.upload_file(&file, static_prefix_ref, progress_ref).await
                })
                .buffer_unordered(MAX_CONCURRENT_UPLOADS)
                .try_collect::<Vec<()>>()
                .await?;

            let run_prefix_ref = &run_prefix;
            stream::iter(run_uploads)
                .map(|file| async move {
                    self.upload_file(&file, run_prefix_ref, progress_ref).await
                })
                .buffer_unordered(MAX_CONCURRENT_UPLOADS)
                .try_collect::<Vec<()>>()
                .await?;

            let published_manifest = build_published_manifest(
                params.local_manifest,
                self.public_snapshot_base_url().as_deref(),
                params.timestamp,
            )?;
            self.upload_manifest(&manifest_key, published_manifest, progress_ref).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        progress_logger.stop();
        if let Err(error) = upload_result {
            error!(
                error = %error,
                run_prefix = %run_prefix,
                manifest_key = %manifest_key,
                static_uploads = static_upload_count,
                run_uploads = run_upload_count,
                skipped,
                "snapshot artifact upload failed"
            );
            return Err(error);
        }

        if let Err(e) = self.prune_old_runs(params.retain_runs).await {
            warn!(error = %e, retain_runs = params.retain_runs, "failed to prune old snapshot runs");
        }

        info!(
            run_prefix = %run_prefix,
            manifest_key = %manifest_key,
            skipped,
            "upload complete"
        );
        Ok(run_prefix)
    }

    /// Returns the `{prefix}/static_files` key prefix.
    fn static_files_prefix(&self) -> String {
        if self.prefix.is_empty() {
            "static_files".to_string()
        } else {
            format!("{}/static_files", self.prefix)
        }
    }

    /// Returns the `{prefix}/{timestamp}` key prefix for a run.
    fn run_prefix(&self, timestamp: u64) -> String {
        if self.prefix.is_empty() {
            timestamp.to_string()
        } else {
            format!("{}/{timestamp}", self.prefix)
        }
    }

    /// Returns the public HTTP base URL for snapshot downloads (snapshot root, not `static_files/`).
    fn public_snapshot_base_url(&self) -> Option<String> {
        let base = self.public_base_url.as_deref()?.trim_end_matches('/');
        Some(if self.prefix.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{}", self.prefix)
        })
    }

    /// Lists all objects under a prefix in the bucket, returning filename → size.
    async fn list_remote_objects(&self, prefix: &str) -> Result<HashMap<String, u64>> {
        let prefix_with_slash = format!("{prefix}/");
        let mut remote = HashMap::new();
        let mut continuation_token = None;

        loop {
            let mut req =
                self.client.list_objects_v2().bucket(&self.bucket).prefix(&prefix_with_slash);

            if let Some(token) = continuation_token.take() {
                req = req.continuation_token(token);
            }

            let resp = req
                .send()
                .await
                .with_context(|| format!("failed to list objects under {prefix_with_slash}"))?;

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    let filename = key.strip_prefix(&prefix_with_slash).unwrap_or(key).to_string();
                    let size: u64 = obj.size.unwrap_or(0).try_into().unwrap_or(0);
                    remote.insert(filename, size);
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(String::from);
            } else {
                break;
            }
        }

        debug!(prefix = %prefix, count = remote.len(), "listed remote objects");
        Ok(remote)
    }

    /// Lists full object keys under a prefix.
    async fn list_remote_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let prefix_with_slash = format!("{prefix}/");
        let mut keys = Vec::new();
        let mut continuation_token = None;

        loop {
            let mut req =
                self.client.list_objects_v2().bucket(&self.bucket).prefix(&prefix_with_slash);

            if let Some(token) = continuation_token.take() {
                req = req.continuation_token(token);
            }

            let resp = req
                .send()
                .await
                .with_context(|| format!("failed to list objects under {prefix_with_slash}"))?;

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    keys.push(key.to_string());
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(String::from);
            } else {
                break;
            }
        }

        Ok(keys)
    }

    /// Returns whether a full object key exists remotely.
    async fn remote_key_exists(&self, key: &str) -> Result<bool> {
        match self.client.head_object().bucket(&self.bucket).key(key).send().await {
            Ok(_) => Ok(true),
            Err(err) if err.as_service_error().is_some_and(|e| e.is_not_found()) => Ok(false),
            Err(err) => Err(anyhow::anyhow!("failed to check object existence for {key}: {err}")),
        }
    }

    /// Returns the remote object size for a full object key when it already exists.
    async fn remote_object_size(&self, key: &str) -> Result<Option<u64>> {
        match self.client.head_object().bucket(&self.bucket).key(key).send().await {
            Ok(resp) => Ok(resp.content_length().and_then(|len| u64::try_from(len).ok())),
            Err(err) if err.as_service_error().is_some_and(|e| e.is_not_found()) => Ok(None),
            Err(err) => Err(anyhow::anyhow!("failed to check object metadata for {key}: {err}")),
        }
    }

    /// Deletes all objects under a prefix and returns the number of deleted keys.
    async fn delete_prefix(&self, prefix: &str) -> Result<usize> {
        let keys = self.list_remote_keys(prefix).await?;
        if keys.is_empty() {
            return Ok(0);
        }

        for batch in keys.chunks(DELETE_OBJECT_BATCH_SIZE) {
            let objects = batch
                .iter()
                .map(|key| ObjectIdentifier::builder().key(key).build())
                .collect::<Result<Vec<_>, _>>()?;
            let delete = Delete::builder().set_objects(Some(objects)).quiet(true).build()?;
            let resp = self
                .client
                .delete_objects()
                .bucket(&self.bucket)
                .delete(delete)
                .send()
                .await
                .with_context(|| format!("failed to delete objects under {prefix}"))?;

            if !resp.errors().is_empty() {
                for err in resp.errors() {
                    warn!(
                        key = %err.key().unwrap_or("<unknown>"),
                        code = %err.code().unwrap_or("<unknown>"),
                        message = %err.message().unwrap_or("<unknown>"),
                        "failed to delete snapshot run object"
                    );
                }
                bail!("failed to delete one or more objects under {prefix}");
            }
        }

        Ok(keys.len())
    }

    /// Uploads a single file, using multipart upload for files above the threshold.
    /// On success, adds the uploaded byte count to `progress` for progress tracking.
    async fn upload_file(
        &self,
        file_path: &Path,
        dest_prefix: &str,
        progress: &UploadProgress,
    ) -> Result<()> {
        let file_name = file_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid file path: {}", file_path.display()))?
            .to_string_lossy();

        let key = format!("{dest_prefix}/{file_name}");
        let file_size = tokio::fs::metadata(file_path).await?.len();

        if file_size > MULTIPART_THRESHOLD {
            debug!(
                key = %key,
                size = file_size,
                part_size = MULTIPART_PART_SIZE,
                parts = file_size.div_ceil(MULTIPART_PART_SIZE),
                "starting multipart upload"
            );
            progress.start_file(key.clone(), file_size, UploadStage::CreatingMultipart);
            match self.upload_multipart(file_path, &key, file_size, progress).await {
                Ok(()) => progress.finish_file(&key),
                Err(error) => {
                    progress.fail_file(&key);
                    return Err(error);
                }
            }
        } else {
            debug!(key = %key, size = file_size, "uploading file");
            progress.start_file(key.clone(), file_size, UploadStage::Uploading);
            match self.upload_single(file_path, &key).await {
                Ok(()) => {
                    progress.add_for_file(&key, file_size);
                    progress.finish_file(&key);
                }
                Err(error) => {
                    progress.fail_file(&key);
                    return Err(error);
                }
            }
        }

        Ok(())
    }

    async fn upload_single(&self, file_path: &Path, key: &str) -> Result<()> {
        retry_upload(
            || async {
                let body = ByteStream::from_path(file_path)
                    .await
                    .with_context(|| format!("failed to read {}", file_path.display()))
                    .map_err(UploadAttemptError::fatal)?;

                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(body)
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|error| UploadAttemptError::retry(error.into()))
            },
            |attempt, error| {
                warn!(
                    key = %key,
                    attempt,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "single-part upload failed, retrying"
                );
            },
            |attempt| {
                info!(key = %key, attempt, "single-part upload succeeded after retrying");
            },
        )
        .await
    }

    async fn upload_multipart(
        &self,
        file_path: &Path,
        key: &str,
        file_size: u64,
        progress: &UploadProgress,
    ) -> Result<()> {
        let create_resp = self.create_multipart_upload_with_retry(key, progress).await?;

        let upload_id = create_resp
            .upload_id()
            .ok_or_else(|| anyhow::anyhow!("no upload_id returned for {key}"))?
            .to_string();
        progress.set_stage(key, UploadStage::Uploading);
        debug!(
            key = %key,
            upload_id,
            size = file_size,
            parts = file_size.div_ceil(MULTIPART_PART_SIZE),
            "multipart upload created"
        );

        let result = self.upload_parts(file_path, key, &upload_id, file_size, progress).await;

        match result {
            Ok(parts) => {
                self.complete_multipart_upload_with_retry(
                    key, &upload_id, file_size, parts, progress,
                )
                .await
            }
            Err(e) => {
                match self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await
                {
                    Ok(_) => {
                        warn!(key = %key, upload_id, "aborted multipart upload after failure")
                    }
                    Err(abort_error) => {
                        error!(
                            key = %key,
                            upload_id,
                            error = %abort_error,
                            error_debug = ?abort_error,
                            "failed to abort multipart upload after failure"
                        );
                    }
                }

                Err(e)
            }
        }
    }

    async fn create_multipart_upload_with_retry(
        &self,
        key: &str,
        progress: &UploadProgress,
    ) -> Result<aws_sdk_s3::operation::create_multipart_upload::CreateMultipartUploadOutput> {
        retry_upload(
            || async {
                progress.set_stage(key, UploadStage::CreatingMultipart);
                self.client
                    .create_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|error| UploadAttemptError::retry(error.into()))
            },
            |attempt, error| {
                warn!(
                    key = %key,
                    attempt,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "multipart upload creation failed, retrying"
                );
            },
            |attempt| {
                info!(key = %key, attempt, "multipart upload creation succeeded after retrying");
            },
        )
        .await
    }

    async fn complete_multipart_upload_with_retry(
        &self,
        key: &str,
        upload_id: &str,
        file_size: u64,
        parts: Vec<CompletedPart>,
        progress: &UploadProgress,
    ) -> Result<()> {
        retry_upload(
            || async {
                progress.set_stage(key, UploadStage::CompletingMultipart);
                let completed =
                    CompletedMultipartUpload::builder().set_parts(Some(parts.clone())).build();
                match self
                    .client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(error) => {
                        let error = anyhow::Error::from(error);
                        if self.remote_object_size(key).await.map_err(UploadAttemptError::fatal)?
                            == Some(file_size)
                        {
                            warn!(
                                key = %key,
                                upload_id,
                                error = %error,
                                error_debug = ?error,
                                size = file_size,
                                "multipart completion response failed after object appeared; treating upload as successful"
                            );
                            Ok(())
                        } else {
                            Err(UploadAttemptError::retry(error))
                        }
                    }
                }
            },
            |attempt, error| {
                warn!(
                    key = %key,
                    upload_id,
                    attempt,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "multipart upload completion failed, retrying"
                );
            },
            |attempt| {
                info!(
                    key = %key,
                    upload_id,
                    attempt,
                    "multipart upload completion succeeded after retrying"
                );
            },
        )
        .await?;
        debug!(key = %key, upload_id, size = file_size, "multipart upload completed");
        Ok(())
    }

    async fn upload_parts(
        &self,
        file_path: &Path,
        key: &str,
        upload_id: &str,
        file_size: u64,
        progress: &UploadProgress,
    ) -> Result<Vec<CompletedPart>> {
        let planned: Vec<(u64, i32)> = std::iter::successors(Some(0u64), |&offset| {
            let next = offset + MULTIPART_PART_SIZE;
            (next < file_size).then_some(next)
        })
        .zip(1i32..)
        .collect();

        if planned.is_empty() {
            bail!("no parts to upload for {key}");
        }

        let mut completed: Vec<CompletedPart> = stream::iter(planned)
            .map(|(offset, part_number)| {
                let length = std::cmp::min(MULTIPART_PART_SIZE, file_size - offset);
                async move {
                    let part = self
                        .upload_single_part(file_path, key, upload_id, part_number, offset, length)
                        .await?;
                    progress.add_for_file(key, length);
                    Ok::<CompletedPart, anyhow::Error>(part)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_UPLOADS)
            .try_collect()
            .await?;

        completed.sort_unstable_by_key(|p| p.part_number);
        Ok(completed)
    }

    async fn upload_single_part(
        &self,
        file_path: &Path,
        key: &str,
        upload_id: &str,
        part_number: i32,
        offset: u64,
        length: u64,
    ) -> Result<CompletedPart> {
        retry_upload(
            || async {
                let body = ByteStream::read_from()
                    .path(file_path)
                    .offset(offset)
                    .length(aws_sdk_s3::primitives::Length::Exact(length))
                    .build()
                    .await
                    .with_context(|| {
                        format!("failed to read part {part_number} of {}", file_path.display())
                    })
                    .map_err(UploadAttemptError::fatal)?;

                let upload_resp = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .body(body)
                    .send()
                    .await
                    .map_err(|error| UploadAttemptError::retry(error.into()))?;

                let e_tag = upload_resp
                    .e_tag()
                    .ok_or_else(|| anyhow::anyhow!("no ETag for part {part_number} of {key}"))
                    .map_err(UploadAttemptError::fatal)?
                    .to_string();

                Ok(CompletedPart::builder().part_number(part_number).e_tag(e_tag).build())
            },
            |attempt, error| {
                warn!(
                    key = %key,
                    upload_id,
                    part_number,
                    attempt,
                    offset,
                    length,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "multipart upload part failed, retrying"
                );
            },
            |attempt| {
                info!(
                    key = %key,
                    upload_id,
                    part_number,
                    attempt,
                    offset,
                    length,
                    "multipart upload part succeeded after retrying"
                );
            },
        )
        .await
    }

    async fn upload_manifest(
        &self,
        manifest_key: &str,
        published_manifest: Vec<u8>,
        progress: &UploadProgress,
    ) -> Result<()> {
        let manifest_len = published_manifest.len() as u64;
        progress.start_file(manifest_key.to_string(), manifest_len, UploadStage::Uploading);
        retry_upload(
            || async {
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(manifest_key)
                    .body(ByteStream::from(published_manifest.clone()))
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|error| UploadAttemptError::retry(error.into()))
            },
            |attempt, error| {
                warn!(
                    key = manifest_key,
                    attempt,
                    error = %error,
                    error_debug = ?error,
                    next_retry_delay_secs = retry_delay_secs(attempt),
                    "manifest upload failed, retrying"
                );
            },
            |attempt| {
                info!(key = manifest_key, attempt, "manifest upload succeeded after retrying");
            },
        )
        .await
        .inspect_err(|_| {
            progress.fail_file(manifest_key);
        })?;
        progress.add_for_file(manifest_key, manifest_len);
        progress.finish_file(manifest_key);
        Ok(())
    }
}

enum UploadAttemptError {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

impl UploadAttemptError {
    const fn retry(error: anyhow::Error) -> Self {
        Self::Retry(error)
    }

    const fn fatal(error: anyhow::Error) -> Self {
        Self::Fatal(error)
    }
}

async fn retry_upload<T, F, Fut, OnRetry, OnSuccess>(
    mut operation: F,
    mut on_retry: OnRetry,
    mut on_success_after_retry: OnSuccess,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, UploadAttemptError>>,
    OnRetry: FnMut(usize, &anyhow::Error),
    OnSuccess: FnMut(usize),
{
    let mut attempt = 1usize;
    loop {
        match operation().await {
            Ok(value) => {
                if attempt > 1 {
                    on_success_after_retry(attempt);
                }
                return Ok(value);
            }
            Err(UploadAttemptError::Retry(error)) => {
                on_retry(attempt, &error);
                sleep(retry_delay(attempt)).await;
                attempt = attempt.saturating_add(1);
            }
            Err(UploadAttemptError::Fatal(error)) => return Err(error),
        }
    }
}

fn retry_delay(attempt: usize) -> Duration {
    UPLOAD_RETRY_DELAY.saturating_mul(attempt as u32).min(MAX_UPLOAD_RETRY_DELAY)
}

fn retry_delay_secs(attempt: usize) -> u64 {
    retry_delay(attempt).as_secs()
}

/// Builds the published manifest for a run.
///
/// `base_url` points at the snapshot root (`{public_base}/{prefix}`). Finalized static-file
/// chunks use `static_files/{archive}` in [`ChunkedArchive::chunk_files`]; tip chunks and
/// state/rocksdb use `{timestamp}/{archive}`. Proofs stays a bare sibling filename for
/// `ProofsDownloader`.
fn build_published_manifest(
    local_manifest: &SnapshotManifest,
    public_snapshot_base_url: Option<&str>,
    timestamp: u64,
) -> Result<Vec<u8>> {
    let mut manifest = local_manifest.clone();
    manifest.base_url = public_snapshot_base_url.map(str::to_owned);

    for (component_name, component) in &mut manifest.components {
        match component {
            ComponentManifest::Single(single)
                if matches!(component_name.as_str(), "state" | "rocksdb_indices") =>
            {
                single.file = format!("{timestamp}/{}", single.file);
            }
            ComponentManifest::Chunked(chunked) => {
                let num_chunks = chunked.num_chunks();
                let mut chunk_files = Vec::with_capacity(num_chunks as usize);
                for i in 0..num_chunks {
                    let start = i
                        .checked_mul(chunked.blocks_per_file)
                        .context("block range overflow in published chunk_files")?;
                    let end = chunked
                        .blocks_per_file
                        .checked_sub(1)
                        .and_then(|offset| start.checked_add(offset))
                        .context("block range overflow in published chunk_files")?;
                    let archive_name = ChunkFilename::format(component_name, start, end);
                    if i.checked_add(1) == Some(num_chunks) {
                        chunk_files.push(format!("{timestamp}/{archive_name}"));
                    } else {
                        chunk_files.push(format!("static_files/{archive_name}"));
                    }
                }
                chunked.chunk_files = chunk_files;
            }
            _ => {}
        }
    }

    Ok(serde_json::to_vec_pretty(&manifest)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_file_chunks_are_diff_eligible() {
        assert_eq!(
            UploadStrategy::classify("headers-0-499999.tar.zst"),
            UploadStrategy::DiffByHash
        );
        assert_eq!(
            UploadStrategy::classify("transactions-500000-999999.tar.zst"),
            UploadStrategy::DiffByHash
        );
        assert_eq!(
            UploadStrategy::classify("receipts-9500000-9999999.tar.zst"),
            UploadStrategy::DiffByHash
        );
        assert_eq!(
            UploadStrategy::classify("account_changesets-0-499999.tar.zst"),
            UploadStrategy::DiffByHash
        );
        assert_eq!(
            UploadStrategy::classify("storage_changesets-1000000-1499999.tar.zst"),
            UploadStrategy::DiffByHash
        );
        assert_eq!(
            UploadStrategy::classify("transaction_senders-0-499999.tar.zst"),
            UploadStrategy::DiffByHash
        );
    }

    #[test]
    fn non_chunk_files_always_upload() {
        assert_eq!(UploadStrategy::classify("state.tar.zst"), UploadStrategy::AlwaysUpload);
        assert_eq!(
            UploadStrategy::classify("rocksdb_indices.tar.zst"),
            UploadStrategy::AlwaysUpload
        );
        assert_eq!(UploadStrategy::classify("proofs.tar.zst"), UploadStrategy::AlwaysUpload);
        assert_eq!(UploadStrategy::classify("manifest.json"), UploadStrategy::AlwaysUpload);
        assert_eq!(UploadStrategy::classify("random-file.txt"), UploadStrategy::AlwaysUpload);
    }

    #[test]
    fn build_published_manifest_sets_chunk_files_and_leaves_proofs_as_sibling() {
        use std::collections::BTreeMap;

        use crate::snapshot::{ChunkedArchive, SingleArchive};

        let mut components = BTreeMap::new();
        components.insert(
            "state".to_string(),
            ComponentManifest::Single(SingleArchive {
                file: "state.tar.zst".to_string(),
                size: 100,
                decompressed_size: 200,
                blake3: None,
                output_files: vec![],
            }),
        );
        components.insert(
            "proofs".to_string(),
            ComponentManifest::Single(SingleArchive {
                file: "proofs.tar.zst".to_string(),
                size: 50,
                decompressed_size: 80,
                blake3: None,
                output_files: vec![],
            }),
        );
        components.insert(
            "headers".to_string(),
            ComponentManifest::Chunked(ChunkedArchive {
                blocks_per_file: 500_000,
                total_blocks: 1_000_000,
                chunk_sizes: vec![100, 200],
                chunk_decompressed_sizes: vec![1_000, 2_000],
                chunk_output_files: vec![vec![], vec![]],
                chunk_files: vec![],
            }),
        );

        let local = SnapshotManifest {
            block: 1_000_000,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 1_700_000_000,
            base_url: None,
            reth_version: None,
            components,
        };

        let published =
            build_published_manifest(&local, Some("https://example.com/mainnet"), 1_700_000_000)
                .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&published).unwrap();

        assert_eq!(
            manifest["base_url"], "https://example.com/mainnet",
            "base_url should point at snapshot root"
        );
        assert_eq!(
            manifest["components"]["state"]["file"], "1700000000/state.tar.zst",
            "state should be rewritten under the timestamp directory"
        );
        assert_eq!(
            manifest["components"]["proofs"]["file"], "proofs.tar.zst",
            "proofs must remain a sibling of manifest.json for ProofsDownloader"
        );
        assert_eq!(
            manifest["components"]["headers"]["chunk_files"],
            serde_json::json!([
                "static_files/headers-0-499999.tar.zst",
                "1700000000/headers-500000-999999.tar.zst",
            ]),
            "headers chunk_files should split finalized and tip paths under root base_url"
        );
    }

    #[test]
    fn classify_chunk_filename_edge_cases() {
        assert_eq!(UploadStrategy::classify("state.tar.zst"), UploadStrategy::AlwaysUpload);
        assert_eq!(UploadStrategy::classify("headers.tar.zst"), UploadStrategy::AlwaysUpload);
        assert_eq!(
            UploadStrategy::classify("headers-abc-def.tar.zst"),
            UploadStrategy::AlwaysUpload
        );
        assert_eq!(
            UploadStrategy::classify("headers-0-499999.tar.gz"),
            UploadStrategy::AlwaysUpload
        );
        assert_eq!(UploadStrategy::classify("headers-0-499999"), UploadStrategy::AlwaysUpload);
        assert_eq!(
            UploadStrategy::classify("headers-0-499999.tar.zst"),
            UploadStrategy::DiffByHash
        );
        assert_eq!(
            UploadStrategy::classify("custom_component-100-200.tar.zst"),
            UploadStrategy::DiffByHash
        );
    }

    #[test]
    fn parse_run_prefix_accepts_timestamp_prefixes() {
        assert_eq!(
            SnapshotUploader::parse_run_prefix("mainnet/1710000002/", "mainnet/"),
            Some((1_710_000_002, "mainnet/1710000002".to_string()))
        );
        assert_eq!(
            SnapshotUploader::parse_run_prefix("1710000002/", ""),
            Some((1_710_000_002, "1710000002".to_string()))
        );
    }

    #[test]
    fn parse_run_prefix_rejects_non_run_prefixes() {
        assert_eq!(SnapshotUploader::parse_run_prefix("mainnet/static_files/", "mainnet/"), None);
        assert_eq!(
            SnapshotUploader::parse_run_prefix("mainnet/1710000002/nested/", "mainnet/"),
            None
        );
        assert_eq!(SnapshotUploader::parse_run_prefix("other/1710000002/", "mainnet/"), None);
    }

    #[test]
    fn expired_run_timestamps_keeps_latest_n() {
        let runs = vec![
            SnapshotRun { timestamp: 10, manifest_key: "10/manifest.json".to_string() },
            SnapshotRun { timestamp: 30, manifest_key: "30/manifest.json".to_string() },
            SnapshotRun { timestamp: 20, manifest_key: "20/manifest.json".to_string() },
            SnapshotRun { timestamp: 40, manifest_key: "40/manifest.json".to_string() },
        ];

        assert_eq!(SnapshotUploader::expired_run_timestamps(&runs, 3), vec![10]);
        assert_eq!(SnapshotUploader::expired_run_timestamps(&runs, 2), vec![20, 10]);
        assert!(SnapshotUploader::expired_run_timestamps(&runs, 4).is_empty());
    }
}
