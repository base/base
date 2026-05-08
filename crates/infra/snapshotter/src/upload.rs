//! S3-compatible upload for snapshot artifacts with diff-based optimization.
//!
//! Static file chunks (e.g. `headers-0-499999.tar.zst`) are immutable for finalized
//! block ranges — only the chunk covering the current tip changes between snapshots.
//! The uploader compares local file sizes against existing remote objects and skips
//! uploads for chunks that already exist with a matching size.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use aws_sdk_s3::{
    Client as S3Client,
    error::SdkError,
    operation::get_object::GetObjectError,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
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
    /// Always upload regardless of remote state (mdbx, rocksdb, proofs).
    AlwaysUpload,
    /// Skip upload if the remote object exists with the same size (static file chunks).
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
/// `{component}-{start}-{end}.tar.zst`. `rocksdb_indices` and `state` do not match
/// this pattern and are classified as `AlwaysUpload`.
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

/// Metadata written to `latest.json` after a successful upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestPointer {
    /// The run prefix where this snapshot's artifacts live.
    pub prefix: String,
    /// Block number of the snapshot.
    pub block: u64,
    /// Unix timestamp of the upload.
    pub timestamp: u64,
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
    /// Reads `latest.json` to find the previous run's prefix and lists objects
    /// under it. Static file chunks that exist in the previous run with the same
    /// size are skipped — they're immutable for finalized block ranges. State,
    /// rocksdb, and manifest are always re-uploaded to the new run prefix.
    /// `manifest.json` is uploaded last, then `latest.json` is updated.
    pub async fn upload(
        &self,
        output_dir: &Path,
        files: &[PathBuf],
        block: u64,
        timestamp: u64,
    ) -> Result<String> {
        let run_prefix = if self.prefix.is_empty() {
            format!("{block}-{timestamp}")
        } else {
            format!("{}/{block}-{timestamp}", self.prefix)
        };

        info!(
            run_prefix = %run_prefix,
            file_count = files.len(),
            bucket = %self.bucket,
            "uploading snapshot artifacts"
        );

        let remote_objects = match self.read_latest_pointer().await? {
            Some(prev) => {
                info!(previous_prefix = %prev.prefix, previous_block = prev.block, "found previous snapshot");
                self.list_remote_objects(&prev.prefix).await?
            }
            None => {
                info!("no previous snapshot found, uploading all files");
                HashMap::new()
            }
        };

        let manifest_path = output_dir.join("manifest.json");
        let mut to_upload = Vec::new();
        let mut skipped = 0u64;

        for file in files {
            if file == &manifest_path {
                continue;
            }

            let file_name =
                file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

            let local_size = std::fs::metadata(file)?.len();
            let strategy = UploadStrategy::classify(&file_name);

            if strategy == UploadStrategy::DiffBySize
                && let Some(&remote_size) = remote_objects.get(&file_name)
            {
                if remote_size == local_size {
                    debug!(file = %file_name, size = local_size, "skipping (remote size matches)");
                    skipped += 1;
                    continue;
                }
                debug!(
                    file = %file_name,
                    local_size,
                    remote_size,
                    "re-uploading (size mismatch)"
                );
            }

            to_upload.push(file.clone());
        }

        info!(uploading = to_upload.len(), skipped, "diff analysis complete");

        stream::iter(to_upload)
            .map(|file| {
                let run_prefix = &run_prefix;
                async move { self.upload_file(&file, run_prefix).await }
            })
            .buffer_unordered(MAX_CONCURRENT_UPLOADS)
            .try_collect::<Vec<()>>()
            .await?;

        if manifest_path.exists() {
            self.upload_file(&manifest_path, &run_prefix).await?;
        }

        let pointer = LatestPointer { prefix: run_prefix.clone(), block, timestamp };
        self.write_latest_pointer(&pointer).await?;

        info!(run_prefix = %run_prefix, block, skipped, "upload complete");
        Ok(run_prefix)
    }

    /// Reads the `latest.json` pointer from the bucket, returning `None` if it doesn't exist.
    async fn read_latest_pointer(&self) -> Result<Option<LatestPointer>> {
        let key = if self.prefix.is_empty() {
            "latest.json".to_string()
        } else {
            format!("{}/latest.json", self.prefix)
        };

        match self.client.get_object().bucket(&self.bucket).key(&key).send().await {
            Ok(resp) => {
                let body = resp.body.collect().await?.into_bytes();
                let pointer: LatestPointer = serde_json::from_slice(&body)
                    .with_context(|| format!("failed to parse {key}"))?;
                Ok(Some(pointer))
            }
            Err(err) => match &err {
                SdkError::ServiceError(e) if matches!(e.err(), GetObjectError::NoSuchKey(_)) => {
                    Ok(None)
                }
                _ => {
                    let err_str = err.to_string();
                    if err_str.contains("NoSuchKey")
                        || err_str.contains("404")
                        || err_str.contains("NotFound")
                    {
                        Ok(None)
                    } else {
                        Err(anyhow::anyhow!(err)).with_context(|| format!("failed to read {key}"))
                    }
                }
            },
        }
    }

    /// Lists all objects under `run_prefix/` in the bucket, returning filename → size.
    async fn list_remote_objects(&self, run_prefix: &str) -> Result<HashMap<String, u64>> {
        let prefix_with_slash = format!("{run_prefix}/");
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
                    let size = obj.size.unwrap_or(0).try_into().unwrap_or(0);
                    remote.insert(filename, size);
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(String::from);
            } else {
                break;
            }
        }

        debug!(count = remote.len(), "listed remote objects");
        Ok(remote)
    }

    /// Uploads a single file, using multipart upload for files above the threshold.
    async fn upload_file(&self, file_path: &Path, run_prefix: &str) -> Result<()> {
        let file_name = file_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid file path: {}", file_path.display()))?
            .to_string_lossy();

        let key = format!("{run_prefix}/{file_name}");
        let file_size = std::fs::metadata(file_path)?.len();

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

    /// Writes the `latest.json` pointer at `{prefix}/latest.json`.
    async fn write_latest_pointer(&self, pointer: &LatestPointer) -> Result<()> {
        let key = if self.prefix.is_empty() {
            "latest.json".to_string()
        } else {
            format!("{}/latest.json", self.prefix)
        };

        let body = serde_json::to_vec_pretty(pointer)?;

        debug!(key = %key, block = pointer.block, "writing latest pointer");

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(ByteStream::from(body))
            .content_type("application/json")
            .send()
            .await
            .with_context(|| format!("failed to write latest pointer at {key}"))?;

        Ok(())
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
        assert_eq!(UploadStrategy::classify("latest.json"), UploadStrategy::AlwaysUpload);
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
