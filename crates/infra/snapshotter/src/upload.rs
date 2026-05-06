//! S3-compatible upload for snapshot artifacts.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use aws_sdk_s3::{
    Client as S3Client,
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

    /// Uploads all files from the output directory to S3 under a unique run prefix.
    ///
    /// The run prefix is `{prefix}/{block}-{timestamp}/`. After all archives are
    /// uploaded, `manifest.json` is uploaded last, and finally `latest.json` is
    /// written at `{prefix}/latest.json` as an atomic pointer to this run.
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

        let manifest_path = output_dir.join("manifest.json");
        let mut non_manifest = Vec::new();

        for file in files {
            if file == &manifest_path {
                continue;
            }
            non_manifest.push(file.clone());
        }

        stream::iter(non_manifest)
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

        info!(run_prefix = %run_prefix, block, "upload complete");
        Ok(run_prefix)
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
