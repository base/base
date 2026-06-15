//! S3-compatible upload for snapshot artifacts with diff-based optimization.
//!
//! Artifacts are split into two areas within the bucket:
//!
//! - `{prefix}/static_files/` — static file chunks that are immutable for finalized
//!   block ranges. Only the tip chunk changes between snapshots. The uploader
//!   compares the per-file BLAKE3 hashes recorded in the previous run's
//!   `manifest.json` against the freshly generated manifest, and skips chunks
//!   whose hashes match.
//!
//! - `{prefix}/{date}/` — per-run directory for mdbx state, rocksdb, and the manifest.
//!   These are always re-uploaded since they change every snapshot.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use aws_sdk_s3::{
    Client as S3Client,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use futures::stream::{self, StreamExt, TryStreamExt};
use tracing::{debug, info, warn};

use crate::snapshot::{ChunkFilename, SnapshotManifest, SnapshotManifestExt};

/// Maximum number of concurrent file uploads.
const MAX_CONCURRENT_UPLOADS: usize = 10;

/// Files larger than this threshold use multipart upload.
/// S3 `put_object` has a 5 `GiB` limit; we switch well below that.
const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Part size for multipart uploads (100 `MiB`).
const MULTIPART_PART_SIZE: u64 = 100 * 1024 * 1024;

/// Determines whether a snapshot component is re-uploaded every run
/// or can be skipped when the remote copy already matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStrategy {
    /// Always upload to the per-run date directory (mdbx, rocksdb, manifest).
    AlwaysUpload,
    /// Upload to `static_files/`, skipping if the per-file BLAKE3 hashes
    /// recorded in the previous run's manifest match the freshly generated ones.
    DiffByHash,
}

impl UploadStrategy {
    /// Classifies a snapshot filename into its upload strategy.
    ///
    /// Static file chunks follow the pattern `{component}-{start}-{end}.tar.zst`
    /// (e.g. `headers-0-499999.tar.zst`). These are immutable for finalized block
    /// ranges and only the tip chunk changes between snapshots.
    ///
    /// Everything else (state, rocksdb, manifest) is always uploaded.
    pub fn classify(filename: &str) -> Self {
        if ChunkFilename::parse(filename).is_some() { Self::DiffByHash } else { Self::AlwaysUpload }
    }
}

/// Uploads snapshot artifacts to an S3-compatible store (R2, `MinIO`, etc.).
#[derive(Debug)]
pub struct SnapshotUploader {
    client: S3Client,
    bucket: String,
    prefix: String,
    public_base_url: Option<String>,
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
        let manifest_suffix = "/manifest.json";
        let list_prefix =
            if self.prefix.is_empty() { String::new() } else { format!("{}/", self.prefix) };

        let mut best: Option<(u64, String)> = None;
        let mut continuation_token = None;

        loop {
            let mut req = self.client.list_objects_v2().bucket(&self.bucket);
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

            for obj in resp.contents() {
                let Some(key) = obj.key() else { continue };
                let Some(rest) = key.strip_prefix(list_prefix.as_str()) else { continue };
                let Some(timestamp_str) = rest.strip_suffix(manifest_suffix) else { continue };
                if timestamp_str.contains('/') {
                    continue;
                }
                let Ok(timestamp) = timestamp_str.parse::<u64>() else { continue };
                if best.as_ref().is_none_or(|(prev, _)| timestamp > *prev) {
                    best = Some((timestamp, key.to_string()));
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(String::from);
            } else {
                break;
            }
        }

        let Some((timestamp, key)) = best else {
            debug!("no previous manifest found");
            return Ok(None);
        };

        debug!(timestamp, key = %key, "fetching previous manifest");
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .with_context(|| format!("failed to fetch previous manifest {key}"))?;

        let bytes = resp
            .body
            .collect()
            .await
            .with_context(|| format!("failed to read previous manifest body {key}"))?
            .into_bytes();

        match serde_json::from_slice::<SnapshotManifest>(&bytes) {
            Ok(manifest) => Ok(Some(manifest)),
            Err(e) => {
                warn!(error = %e, key = %key, "failed to parse previous manifest, treating as missing");
                Ok(None)
            }
        }
    }

    /// Uploads snapshot artifacts with diff-based optimization.
    ///
    /// Static file chunks go to `{prefix}/static_files/` and are skipped when
    /// their per-file BLAKE3 hashes (recorded in `local_manifest`) match those
    /// from `remote_manifest`. State, rocksdb, and manifest go to
    /// `{prefix}/{timestamp}/` and are always re-uploaded. `manifest.json` is
    /// uploaded last as the "snapshot complete" signal.
    pub async fn upload(
        &self,
        output_dir: &Path,
        files: &[PathBuf],
        timestamp: u64,
        local_manifest: &SnapshotManifest,
        remote_manifest: Option<&SnapshotManifest>,
    ) -> Result<String> {
        let static_prefix = self.static_files_prefix();
        let run_prefix = self.run_prefix(timestamp);

        info!(
            run_prefix = %run_prefix,
            static_prefix = %static_prefix,
            file_count = files.len(),
            bucket = %self.bucket,
            "uploading snapshot artifacts"
        );

        let manifest_path = output_dir.join("manifest.json");
        let mut static_uploads = Vec::new();
        let mut run_uploads = Vec::new();
        let mut skipped = 0u64;

        for file in files {
            if file == &manifest_path {
                continue;
            }

            let file_name = file
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("invalid file path: {}", file.display()))?
                .to_string_lossy()
                .to_string();

            let strategy = UploadStrategy::classify(&file_name);

            match strategy {
                UploadStrategy::DiffByHash => {
                    let local_hashes = local_manifest.chunk_hashes_for_file(&file_name);
                    let remote_hashes =
                        remote_manifest.and_then(|m| m.chunk_hashes_for_file(&file_name));
                    match (&local_hashes, &remote_hashes) {
                        (Some(local), Some(remote)) if local == remote => {
                            debug!(file = %file_name, "skipping static file (blake3 matches)");
                            skipped += 1;
                            continue;
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
                UploadStrategy::AlwaysUpload => {
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

        let static_prefix_ref = &static_prefix;
        stream::iter(static_uploads)
            .map(|file| async move { self.upload_file(&file, static_prefix_ref).await })
            .buffer_unordered(MAX_CONCURRENT_UPLOADS)
            .try_collect::<Vec<()>>()
            .await?;

        let run_prefix_ref = &run_prefix;
        stream::iter(run_uploads)
            .map(|file| async move { self.upload_file(&file, run_prefix_ref).await })
            .buffer_unordered(MAX_CONCURRENT_UPLOADS)
            .try_collect::<Vec<()>>()
            .await?;

        let published_manifest = build_published_manifest(
            local_manifest,
            self.public_static_files_base_url().as_deref(),
            timestamp,
        )?;
        let manifest_key = format!("{run_prefix}/manifest.json");
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&manifest_key)
            .body(ByteStream::from(published_manifest))
            .send()
            .await
            .with_context(|| format!("failed to upload {manifest_key}"))?;

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

    /// Returns the public base URL for top-level static files, if configured.
    fn public_static_files_base_url(&self) -> Option<String> {
        let base = self.public_base_url.as_deref()?.trim_end_matches('/');
        Some(if self.prefix.is_empty() {
            format!("{base}/static_files")
        } else {
            format!("{base}/{}/static_files", self.prefix)
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

    /// Uploads a single file, using multipart upload for files above the threshold.
    async fn upload_file(&self, file_path: &Path, dest_prefix: &str) -> Result<()> {
        let file_name = file_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid file path: {}", file_path.display()))?
            .to_string_lossy();

        let key = format!("{dest_prefix}/{file_name}");
        let file_size = tokio::fs::metadata(file_path).await?.len();

        if file_size > MULTIPART_THRESHOLD {
            debug!(key = %key, size = file_size, "uploading file (multipart)");
            self.upload_multipart(file_path, &key, file_size).await
        } else {
            debug!(key = %key, size = file_size, "uploading file");
            self.upload_single(file_path, &key).await
        }
    }

    async fn upload_single(&self, file_path: &Path, key: &str) -> Result<()> {
        let body = ByteStream::from_path(file_path)
            .await
            .with_context(|| format!("failed to read {}", file_path.display()))?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .with_context(|| format!("failed to upload {key}"))?;

        Ok(())
    }

    async fn upload_multipart(&self, file_path: &Path, key: &str, file_size: u64) -> Result<()> {
        let create_resp = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("failed to initiate multipart upload for {key}"))?;

        let upload_id = create_resp
            .upload_id()
            .ok_or_else(|| anyhow::anyhow!("no upload_id returned for {key}"))?
            .to_string();

        let result = self.upload_parts(file_path, key, &upload_id, file_size).await;

        match result {
            Ok(parts) => {
                let completed = CompletedMultipartUpload::builder().set_parts(Some(parts)).build();

                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                    .with_context(|| format!("failed to complete multipart upload for {key}"))?;

                Ok(())
            }
            Err(e) => {
                self.client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await
                    .ok();

                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        file_path: &Path,
        key: &str,
        upload_id: &str,
        file_size: u64,
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
                    self.upload_single_part(file_path, key, upload_id, part_number, offset, length)
                        .await
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
        let body = ByteStream::read_from()
            .path(file_path)
            .offset(offset)
            .length(aws_sdk_s3::primitives::Length::Exact(length))
            .build()
            .await
            .with_context(|| {
                format!("failed to read part {part_number} of {}", file_path.display())
            })?;

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
            .with_context(|| format!("failed to upload part {part_number} of {key}"))?;

        let e_tag = upload_resp
            .e_tag()
            .ok_or_else(|| anyhow::anyhow!("no ETag for part {part_number} of {key}"))?
            .to_string();

        Ok(CompletedPart::builder().part_number(part_number).e_tag(e_tag).build())
    }
}

/// Builds the single published manifest for a run.
///
/// Chunked archives are served from top-level `static_files/` via `base_url`, while
/// the always-changing `state` and `rocksdb_indices` archives stay in the timestamped
/// run directory and are referenced through `../{timestamp}/...` file paths.
fn build_published_manifest(
    local_manifest: &SnapshotManifest,
    public_static_files_base_url: Option<&str>,
    timestamp: u64,
) -> Result<Vec<u8>> {
    let mut manifest = local_manifest.clone();
    manifest.base_url = public_static_files_base_url.map(str::to_owned);

    for (component_name, component) in &mut manifest.components {
        if let reth_cli_commands::download::manifest::ComponentManifest::Single(single) = component
            && matches!(component_name.as_str(), "state" | "rocksdb_indices")
        {
            single.file = format!("../{timestamp}/{}", single.file);
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
        assert_eq!(UploadStrategy::classify("manifest.json"), UploadStrategy::AlwaysUpload);
        assert_eq!(UploadStrategy::classify("random-file.txt"), UploadStrategy::AlwaysUpload);
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
}
