//! Upload, download and deletion of segment objects under `seg/<k>/<epoch>/`.

use std::{path::Path, sync::Arc};

use alloy_primitives::B256;
use futures::TryStreamExt;
use object_store::{ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{
    ArchiveLayout, ArchiveReader, BlockData, HistoryStoreError, ManifestEntry, SegmentCache,
    SegmentFile, SegmentFiles, SegmentManifest,
};

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
}

impl SegmentObjects {
    /// Creates a handle.
    pub const fn new(store: Arc<dyn ObjectStore>, layout: ArchiveLayout) -> Self {
        Self { store, layout }
    }

    /// Uploads every file under writer epoch `epoch` with `If-None-Match: *`. An existing object
    /// counts as success when its bytes equal the new build (builds are deterministic, and only
    /// this epoch's owner writes under its directory); different bytes are an error.
    pub async fn upload(
        &self,
        files: &SegmentFiles,
        epoch: u64,
    ) -> Result<UploadReport, HistoryStoreError> {
        let mut report = UploadReport::default();
        for (name, bytes) in &files.files {
            let path = self.layout.segment_object(files.segment, epoch, name);
            let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
            match self.store.put_opts(&path, PutPayload::from(bytes.0.clone()), opts).await {
                Ok(_) => report.created += 1,
                Err(object_store::Error::AlreadyExists { .. }) => {
                    let existing = self.store.get(&path).await?.bytes().await?;
                    if existing != bytes.0 {
                        return Err(HistoryStoreError::CorruptSegment {
                            segment: files.segment,
                            file: name.clone(),
                            reason: "existing object differs from deterministic rebuild"
                                .to_string(),
                        });
                    }
                    report.already_present += 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(report)
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
