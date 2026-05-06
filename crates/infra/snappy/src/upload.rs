//! S3-compatible upload for snapshot artifacts.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

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
    pub async fn upload(&self, output_dir: &Path, files: &[PathBuf], block: u64) -> Result<String> {
        let timestamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();

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

        for file in &non_manifest {
            self.upload_file(file, &run_prefix).await?;
        }

        if manifest_path.exists() {
            self.upload_file(&manifest_path, &run_prefix).await?;
        }

        let pointer = LatestPointer { prefix: run_prefix.clone(), block, timestamp };
        self.write_latest_pointer(&pointer).await?;

        info!(run_prefix = %run_prefix, block, "upload complete");
        Ok(run_prefix)
    }

    /// Uploads a single file to S3.
    async fn upload_file(&self, file_path: &Path, run_prefix: &str) -> Result<()> {
        let file_name = file_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid file path: {}", file_path.display()))?
            .to_string_lossy();

        let key = format!("{run_prefix}/{file_name}");
        let body = ByteStream::from_path(file_path)
            .await
            .with_context(|| format!("failed to read file {}", file_path.display()))?;

        let file_size = std::fs::metadata(file_path)?.len();

        debug!(
            key = %key,
            size = file_size,
            "uploading file"
        );

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(body)
            .send()
            .await
            .with_context(|| format!("failed to upload {key} to S3"))?;

        Ok(())
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
