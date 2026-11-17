//! Upload, download and deletion of segment objects under `seg/<k>/<epoch>/`.

use std::{path::Path, sync::Arc};

use alloy_primitives::B256;
use futures::TryStreamExt;
use object_store::{
    MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt, path::Path as ObjectPath,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

use crate::{
    ArchiveLayout, ArchiveReader, BlockData, BuiltFile, BuiltSegment, HistoryStoreError,
    ManifestEntry, SegmentCache, SegmentFile, SegmentFiles, SegmentManifest,
};

/// Default multipart part size for segment uploads (8 `MiB`). S3 requires every part but the
/// last to be at least 5 `MiB`.
pub const DEFAULT_UPLOAD_PART_SIZE: usize = 8 * 1024 * 1024;

/// Outcome of uploading one segment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadReport {
    /// Objects created by this upload.
    pub created: usize,
    /// Objects that already existed with identical bytes (an earlier run uploaded them).
    pub already_present: usize,
}

/// Segment objects in the archive bucket.
#[derive(Debug, Clone)]
pub struct SegmentObjects {
    /// Bucket.
    pub store: Arc<dyn ObjectStore>,
    /// Layout.
    pub layout: ArchiveLayout,
    /// Multipart part size of segment uploads, in bytes.
    pub part_size: usize,
}

impl SegmentObjects {
    /// Creates a handle with [`DEFAULT_UPLOAD_PART_SIZE`] parts.
    pub const fn new(store: Arc<dyn ObjectStore>, layout: ArchiveLayout) -> Self {
        Self { store, layout, part_size: DEFAULT_UPLOAD_PART_SIZE }
    }

    /// Uploads every file of `built` under writer epoch `epoch`, streaming each from disk in
    /// `part_size` parts. An existing object counts as success when its size and SHA-256 equal
    /// the new build's (builds are deterministic); a different object is `CorruptSegment`.
    ///
    /// `object_store` multipart uploads cannot be conditional (`If-None-Match: *` is not sent on
    /// completion), so each object is checked with HEAD first and then written unconditionally.
    /// This is safe because the path lies in this writer's own `seg/<k>/<epoch>/` directory:
    /// only the owner of `epoch` writes there, a superseded writer can only write into its own
    /// older epoch directory (never this one), and those older unreferenced directories are
    /// removed by [`Self::delete_older_epochs`] and orphan GC. A multipart object becomes
    /// visible only on completion, so a crash mid-upload leaves no partial object, only parts
    /// that a failed upload aborts on a best-effort basis.
    pub async fn upload(
        &self,
        built: &BuiltSegment,
        epoch: u64,
    ) -> Result<UploadReport, HistoryStoreError> {
        let mut report = UploadReport::default();
        for (name, file) in &built.files {
            let path = self.layout.segment_object(built.segment, epoch, name);
            match self.store.head(&path).await {
                Ok(meta) => {
                    self.check_existing(built.segment, name, &path, meta.size, file).await?;
                    report.already_present += 1;
                }
                Err(object_store::Error::NotFound { .. }) => {
                    self.upload_file(&file.path, &path).await?;
                    report.created += 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(report)
    }

    /// Checks that the existing object at `path` (of `size` bytes) is the one built: same size
    /// and the same SHA-256, hashed as its chunks arrive.
    pub async fn check_existing(
        &self,
        segment: u64,
        name: &str,
        path: &ObjectPath,
        size: u64,
        file: &BuiltFile,
    ) -> Result<(), HistoryStoreError> {
        let differs = || HistoryStoreError::CorruptSegment {
            segment,
            file: name.to_string(),
            reason: "existing object differs from deterministic rebuild".to_string(),
        };
        if size != file.entry.size {
            return Err(differs());
        }
        let mut chunks = self.store.get(path).await?.into_stream();
        let mut hasher = Sha256::new();
        while let Some(chunk) = chunks.try_next().await? {
            hasher.update(&chunk);
        }
        if B256::from_slice(&hasher.finalize()) != file.entry.sha256 {
            return Err(differs());
        }
        Ok(())
    }

    /// Streams the local file `local` to `location` as a multipart upload of `part_size`
    /// parts, one part in memory at a time. On failure the upload is aborted (best effort).
    pub async fn upload_file(
        &self,
        local: &Path,
        location: &ObjectPath,
    ) -> Result<(), HistoryStoreError> {
        let mut upload = self.store.put_multipart(location).await?;
        let sent = match Self::send_parts(upload.as_mut(), local, self.part_size).await {
            Ok(()) => upload.complete().await.map(drop).map_err(HistoryStoreError::from),
            Err(err) => Err(err),
        };
        if let Err(err) = sent {
            if let Err(abort) = upload.abort().await {
                warn!(path = %location, error = %abort, "aborting a failed multipart upload failed");
            }
            return Err(err);
        }
        Ok(())
    }

    /// Reads `local` in `part_size` parts and uploads each before reading the next.
    pub async fn send_parts(
        upload: &mut dyn MultipartUpload,
        local: &Path,
        part_size: usize,
    ) -> Result<(), HistoryStoreError> {
        let mut file = tokio::fs::File::open(local).await?;
        loop {
            let mut part = Vec::with_capacity(part_size);
            (&mut file).take(part_size as u64).read_to_end(&mut part).await?;
            if part.is_empty() {
                return Ok(());
            }
            let full = part.len() == part_size;
            upload.put_part(part.into()).await?;
            if !full {
                return Ok(());
            }
        }
    }

    /// Lists the objects of segment `k` under every epoch.
    pub async fn list(&self, k: u64) -> Result<Vec<ObjectMeta>, HistoryStoreError> {
        Ok(self.store.list(Some(&self.layout.segment_dir(k))).try_collect().await?)
    }

    /// Lists every object under `seg/` with its segment number.
    pub async fn list_all(&self) -> Result<Vec<(u64, ObjectMeta)>, HistoryStoreError> {
        let metas: Vec<ObjectMeta> =
            self.store.list(Some(&self.layout.segments())).try_collect().await?;
        Ok(metas
            .into_iter()
            .filter_map(|meta| self.layout.parse_segment(&meta.location).map(|k| (k, meta)))
            .collect())
    }

    /// Downloads the objects `manifest` lists from its epoch directory into memory. A missing
    /// object means the directory was swept or never completed, so it is `Unavailable`;
    /// checksums are left to [`SegmentManifest::check`]. Test-only: production paths stream
    /// with [`Self::download_to`] or [`Self::read_blocks`].
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn download(
        &self,
        manifest: &SegmentManifest,
        blocks_per_segment: u64,
    ) -> Result<SegmentFiles, HistoryStoreError> {
        let k = manifest.segment;
        let mut files = std::collections::BTreeMap::new();
        for name in manifest.files.keys() {
            let path = self.layout.segment_object(k, manifest.epoch, name);
            let bytes: alloy_primitives::Bytes = match self.store.get(&path).await {
                Ok(result) => result.bytes().await?.into(),
                Err(object_store::Error::NotFound { .. }) => {
                    return Err(HistoryStoreError::Unavailable {
                        reason: format!("segment {k} object {name} missing"),
                    });
                }
                Err(err) => return Err(err.into()),
            };
            files.insert(name.clone(), bytes);
        }
        if !files.contains_key(SegmentFile::BODY_INDICES) {
            return Err(HistoryStoreError::Unavailable {
                reason: format!("segment {k} objects missing"),
            });
        }
        Ok(SegmentFiles { segment: k, blocks_per_segment, files })
    }

    /// Streams the objects `manifest` lists into `dir` under their local names
    /// ([`SegmentManifest::local_name`]), checking each object's size and SHA-256 as its chunks
    /// arrive. Memory stays bounded by one chunk per object whatever the segment size, and an
    /// object longer than its manifest size is rejected before the rest of it is read. A missing
    /// object is `Unavailable`; a size or checksum mismatch is `CorruptSegment`. On error, `dir`
    /// may hold partial files and the caller discards it.
    pub async fn download_to(
        &self,
        manifest: &SegmentManifest,
        blocks_per_segment: u64,
        dir: &Path,
    ) -> Result<(), HistoryStoreError> {
        let k = manifest.segment;
        manifest.check_range(k, blocks_per_segment)?;
        if !manifest.files.contains_key(SegmentFile::BODY_INDICES) {
            return Err(HistoryStoreError::Unavailable {
                reason: format!("segment {k} objects missing"),
            });
        }
        for (name, expected) in &manifest.files {
            let local = manifest
                .local_name(name, blocks_per_segment)
                .ok_or_else(|| ManifestEntry::corrupt(k, name, "not a segment file".to_string()))?;
            let path = self.layout.segment_object(k, manifest.epoch, name);
            let mut chunks = match self.store.get(&path).await {
                Ok(result) => result.into_stream(),
                Err(object_store::Error::NotFound { .. }) => {
                    return Err(HistoryStoreError::Unavailable {
                        reason: format!("segment {k} object {name} missing"),
                    });
                }
                Err(err) => return Err(err.into()),
            };
            let mut file = tokio::fs::File::create(dir.join(local)).await?;
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            while let Some(chunk) = chunks.try_next().await? {
                size += chunk.len() as u64;
                if size > expected.size {
                    return Err(ManifestEntry::corrupt(
                        k,
                        name,
                        format!("more than manifest {} bytes", expected.size),
                    ));
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            expected.check(k, name, size, B256::from_slice(&hasher.finalize()))?;
        }
        Ok(())
    }

    /// Reads every block of the segment `manifest` describes: streams its checked objects into
    /// a temporary directory ([`Self::download_to`]) and decodes them on the blocking pool, so
    /// memory holds one chunk per object plus the decoded blocks, never the segment's files.
    pub async fn read_blocks(
        &self,
        manifest: &SegmentManifest,
        blocks_per_segment: u64,
    ) -> Result<Vec<BlockData>, HistoryStoreError> {
        let k = manifest.segment;
        let dir = ArchiveReader::blocking(|| Ok(tempfile::tempdir()?)).await?;
        if let Err(err) = self.download_to(manifest, blocks_per_segment, dir.path()).await {
            return SegmentCache::abandon(dir, err).await;
        }
        ArchiveReader::blocking(move || {
            let blocks = SegmentFiles::read_blocks_in(k, blocks_per_segment, dir.path());
            dir.close()?;
            blocks
        })
        .await
    }

    /// Deletes the objects of segment `k` under every epoch older than `epoch`, returning how
    /// many were deleted. Only the owner of `epoch` calls this, for a segment above `H`, so no
    /// published manifest references the deleted directories.
    pub async fn delete_older_epochs(
        &self,
        k: u64,
        epoch: u64,
    ) -> Result<usize, HistoryStoreError> {
        let mut deleted = 0;
        for meta in self.list(k).await? {
            if self.layout.parse_object(&meta.location).is_some_and(|(_, older)| older < epoch) {
                self.store.delete(&meta.location).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Deletes every object of segment `k` under every epoch, returning how many were deleted.
    pub async fn delete(&self, k: u64) -> Result<usize, HistoryStoreError> {
        let metas = self.list(k).await?;
        for meta in &metas {
            self.store.delete(&meta.location).await?;
        }
        Ok(metas.len())
    }
}
