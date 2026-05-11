//! S3-compatible upload for snapshot artifacts with diff-based optimization.
//!
//! Artifacts are split into two areas within the bucket:
//!
//! - `{prefix}/static_files/` — static file chunks that are immutable for finalized
//!   block ranges. Only the tip chunk changes between snapshots. The uploader
//!   compares local sizes against existing remote objects and skips unchanged chunks.
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
use tracing::{debug, info};

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
    /// Upload to `static_files/`, skipping if the remote object has the same size.
    DiffBySize,
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
        if is_static_file_chunk(filename) { Self::DiffBySize } else { Self::AlwaysUpload }
    }
}

/// Returns `true` if the filename matches the static file chunk pattern:
/// `{component}-{start}-{end}.tar.zst`.
fn is_static_file_chunk(filename: &str) -> bool {
    let Some(stem) = filename.strip_suffix(".tar.zst") else {
        return false;
    };

    let parts: Vec<&str> = stem.rsplitn(3, '-').collect();
    if parts.len() < 3 {
        return false;
    }

    let end_ok = parts[0].parse::<u64>().is_ok();
    let start_ok = parts[1].parse::<u64>().is_ok();
    end_ok && start_ok
}

/// Uploads snapshot artifacts to an S3-compatible store (R2, `MinIO`, etc.).
#[derive(Debug)]
pub struct SnapshotUploader {
    client: S3Client,
    bucket: String,
    prefix: String,
}

impl SnapshotUploader {
    /// Creates a new uploader.
    pub const fn new(client: S3Client, bucket: String, prefix: String) -> Self {
        Self { client, bucket, prefix }
    }

    /// Uploads snapshot artifacts with diff-based optimization.
    ///
    /// Static file chunks go to `{prefix}/static_files/` and are skipped if the
    /// remote object already exists with the same size. State, rocksdb, and
    /// manifest go to `{prefix}/{date}/` and are always re-uploaded.
    /// `manifest.json` is uploaded last as the "snapshot complete" signal.
    pub async fn upload(
        &self,
        output_dir: &Path,
        files: &[PathBuf],
        timestamp: u64,
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

        let remote_static_files = self.list_remote_objects(&static_prefix).await?;

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

            let local_size = tokio::fs::metadata(file).await?.len();
            let strategy = UploadStrategy::classify(&file_name);

            match strategy {
                UploadStrategy::DiffBySize => {
                    if let Some(&remote_size) = remote_static_files.get(&file_name) {
                        if remote_size == local_size {
                            debug!(file = %file_name, size = local_size, "skipping static file (size matches)");
                            skipped += 1;
                            continue;
                        }
                        debug!(file = %file_name, local_size, remote_size, "re-uploading static file (size mismatch)");
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

        if manifest_path.exists() {
            self.upload_file(&manifest_path, &run_prefix).await?;
        }

        info!(run_prefix = %run_prefix, skipped, "upload complete");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_file_chunks_are_diff_eligible() {
        assert_eq!(
            UploadStrategy::classify("headers-0-499999.tar.zst"),
            UploadStrategy::DiffBySize
        );
        assert_eq!(
            UploadStrategy::classify("transactions-500000-999999.tar.zst"),
            UploadStrategy::DiffBySize
        );
        assert_eq!(
            UploadStrategy::classify("receipts-9500000-9999999.tar.zst"),
            UploadStrategy::DiffBySize
        );
        assert_eq!(
            UploadStrategy::classify("account_changesets-0-499999.tar.zst"),
            UploadStrategy::DiffBySize
        );
        assert_eq!(
            UploadStrategy::classify("storage_changesets-1000000-1499999.tar.zst"),
            UploadStrategy::DiffBySize
        );
        assert_eq!(
            UploadStrategy::classify("transaction_senders-0-499999.tar.zst"),
            UploadStrategy::DiffBySize
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
    fn is_static_file_chunk_edge_cases() {
        assert!(!is_static_file_chunk("state.tar.zst"));
        assert!(!is_static_file_chunk("headers.tar.zst"));
        assert!(!is_static_file_chunk("headers-abc-def.tar.zst"));
        assert!(!is_static_file_chunk("headers-0-499999.tar.gz"));
        assert!(!is_static_file_chunk("headers-0-499999"));
        assert!(is_static_file_chunk("headers-0-499999.tar.zst"));
        assert!(is_static_file_chunk("custom_component-100-200.tar.zst"));
    }
}
