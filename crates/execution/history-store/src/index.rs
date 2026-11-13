//! `SlateDB` index under `idx/` (TDD §2.3).
//!
//! The writer opens the database with the WAL disabled and issues one write batch and one
//! memtable flush per segment. Readers open a `DbReader` that follows the latest manifest and
//! must ignore entries for blocks above the catalog's `H`.
//!
//! Index freshness: a reader's `SlateDB` view trails the writer by up to one manifest poll, so
//! it can see a catalog `H` whose index entries it does not see yet. Each segment batch also
//! writes the watermark `w` = the segment's last block. `SlateDB` applies a `WriteBatch`
//! atomically (`slatedb-0.18.1` `src/batch.rs:23`, `Db::write` at `src/db.rs:1650`, covered by
//! `test_mixed_batch_publishes_atomically` at `src/db.rs:11990`), so a view with watermark
//! `≥ H` holds every entry up to `H`. Readers treat an absent entry as authoritative only then.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use alloy_primitives::B256;
use object_store::ObjectStore;
use roaring::RoaringBitmap;
use slatedb::{
    Db, DbReader, DbReaderMode, WriteBatch,
    config::{DbReaderOptions, Settings},
};

use crate::{
    ArchiveLayout, BlockData, HashedKey, HistoryStoreError, IndexKey, SegmentManifest,
    WATERMARK_KEY,
};

/// Every index entry contributed by one segment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentIndex {
    /// Transaction hash → (block, index in block).
    pub txs: Vec<(B256, u64, u32)>,
    /// Block hash → block number.
    pub blocks: Vec<(B256, u64)>,
    /// Hashed state key → offsets (block − k·N) of blocks in this segment that changed it.
    pub history: BTreeMap<HashedKey, RoaringBitmap>,
    /// Segment number.
    pub segment: u64,
}

impl SegmentIndex {
    /// Computes the entries of segment `segment` from its blocks (hashes recomputed locally).
    pub fn from_blocks(segment: u64, blocks_per_segment: u64, blocks: &[BlockData]) -> Self {
        let first = segment * blocks_per_segment;
        let mut index = Self { segment, ..Default::default() };
        for block in blocks {
            let number = block.number();
            let offset = (number - first) as u32;
            index.blocks.push((block.hash, number));
            for (i, tx) in block.transactions.iter().enumerate() {
                index.txs.push((tx.tx_hash(), number, i as u32));
            }
            for change in &block.account_changes {
                index.history.entry(HashedKey::account(change.address)).or_default().insert(offset);
            }
            for change in &block.storage_changes {
                index
                    .history
                    .entry(HashedKey::storage(change.address, change.key))
                    .or_default()
                    .insert(offset);
            }
        }
        index
    }

    /// Key/value pairs to write.
    pub fn puts(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::with_capacity(self.txs.len() + self.blocks.len() + self.history.len());
        out.extend(
            self.txs
                .iter()
                .map(|(hash, block, idx)| (IndexKey::tx(*hash), IndexKey::tx_value(*block, *idx))),
        );
        out.extend(
            self.blocks
                .iter()
                .map(|(hash, block)| (IndexKey::block(*hash), IndexKey::block_value(*block))),
        );
        out.extend(self.history.iter().map(|(key, bitmap)| {
            (IndexKey::history(key, self.segment), IndexKey::bitmap_value(bitmap))
        }));
        out
    }

    /// Keys to tombstone when the segment is retired.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        self.puts().into_iter().map(|(key, _)| key).collect()
    }
}

/// Options for opening the index.
#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    /// Run `SlateDB`'s garbage collector. Must be `false` on `LocalFileSystem`, whose lack of
    /// conditional overwrites breaks GC boundary files.
    pub gc_enabled: bool,
    /// How often readers poll for a new manifest.
    pub manifest_poll_interval: Duration,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self { gc_enabled: true, manifest_poll_interval: Duration::from_secs(1) }
    }
}

/// The single index writer. Opening it claims a new `SlateDB` writer epoch, which fences any
/// previous writer.
pub struct IndexWriter {
    /// Open database.
    pub db: Db,
}

impl std::fmt::Debug for IndexWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexWriter").finish_non_exhaustive()
    }
}

impl IndexWriter {
    /// Opens (or creates) the index with the WAL disabled.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        layout: &ArchiveLayout,
        options: IndexOptions,
    ) -> Result<Self, HistoryStoreError> {
        let mut settings = Settings { wal_enabled: false, ..Default::default() };
        if !options.gc_enabled {
            settings.garbage_collector_options = None;
        }
        let db = Db::builder(layout.index(), store).with_settings(settings).build().await?;
        Ok(Self { db })
    }

    /// Returns the `SlateDB` writer epoch this instance claimed when it opened; the catalog
    /// ownership token is set to the same number.
    pub fn epoch(&self) -> u64 {
        self.db.manifest().writer_epoch()
    }

    /// Returns the checksum manifest of segment `k` from this writer's own (always current)
    /// view, or `None` when the segment has no index entries.
    pub async fn manifest(&self, k: u64) -> Result<Option<SegmentManifest>, HistoryStoreError> {
        self.db
            .get(IndexKey::manifest(k))
            .await?
            .map(|raw| SegmentManifest::decode(k, &raw))
            .transpose()
    }

    /// Returns the checksum manifest of published segment `k`; a missing manifest is corrupt.
    pub async fn required_manifest(&self, k: u64) -> Result<SegmentManifest, HistoryStoreError> {
        self.manifest(k).await?.ok_or_else(|| HistoryStoreError::CorruptSegment {
            segment: k,
            file: "manifest".to_string(),
            reason: "missing".to_string(),
        })
    }

    /// Writes one segment's entries, its checksum manifest and the watermark (the manifest's
    /// last block) in a single atomic batch, then flushes the memtable.
    pub async fn write_segment(
        &self,
        index: &SegmentIndex,
        manifest: &SegmentManifest,
    ) -> Result<(), HistoryStoreError> {
        let mut batch = WriteBatch::new();
        for (key, value) in index.puts() {
            batch.put(key, value);
        }
        batch.put(IndexKey::manifest(manifest.segment), manifest.encode());
        batch.put(WATERMARK_KEY, manifest.last_block.to_be_bytes());
        self.db.write(batch).await?;
        self.db.flush().await?;
        Ok(())
    }

    /// Tombstones keys in a single batch and flushes the memtable.
    pub async fn delete_keys(&self, keys: &[Vec<u8>]) -> Result<(), HistoryStoreError> {
        let mut batch = WriteBatch::new();
        for key in keys {
            batch.delete(key);
        }
        self.db.write(batch).await?;
        self.db.flush().await?;
        Ok(())
    }

    /// Closes the database.
    pub async fn close(&self) -> Result<(), HistoryStoreError> {
        Ok(self.db.close().await?)
    }
}

/// Read-only view of the index.
pub struct IndexReader {
    /// Open reader.
    pub reader: DbReader,
}

impl std::fmt::Debug for IndexReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexReader").finish_non_exhaustive()
    }
}

impl IndexReader {
    /// Opens a reader that follows the latest manifest and skips WAL replay.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        layout: &ArchiveLayout,
        options: IndexOptions,
    ) -> Result<Self, HistoryStoreError> {
        let reader_options = DbReaderOptions {
            manifest_poll_interval: options.manifest_poll_interval,
            skip_wal_replay: true,
            ..Default::default()
        };
        let reader =
            DbReader::open(layout.index(), store, DbReaderMode::FollowLatest, reader_options)
                .await?;
        Ok(Self { reader })
    }

    /// Looks up a transaction's (block, index), ignoring entries above `h`.
    pub async fn tx_location(
        &self,
        hash: B256,
        h: u64,
    ) -> Result<Option<(u64, u32)>, HistoryStoreError> {
        let Some(raw) = self.reader.get(IndexKey::tx(hash)).await? else { return Ok(None) };
        let location = IndexKey::decode_tx_value(&raw)?;
        Ok((location.0 <= h).then_some(location))
    }

    /// Returns the last block whose entries this reader's view holds, or `None` before the
    /// first segment.
    pub async fn watermark(&self) -> Result<Option<u64>, HistoryStoreError> {
        self.reader
            .get(WATERMARK_KEY)
            .await?
            .map(|raw| IndexKey::decode_block_value(&raw))
            .transpose()
    }

    /// Fails with `Unavailable` unless this reader's view covers every entry up to `h`, so an
    /// absent entry can be trusted.
    pub async fn ensure_covers(&self, h: u64) -> Result<(), HistoryStoreError> {
        let watermark = self.watermark().await?;
        if watermark.is_none_or(|watermark| watermark < h) {
            return Err(HistoryStoreError::Unavailable {
                reason: format!("index view at {watermark:?} trails archive H {h}"),
            });
        }
        Ok(())
    }

    /// Returns the checksum manifest of segment `k`; a missing manifest is a corrupt segment.
    pub async fn manifest(&self, k: u64) -> Result<SegmentManifest, HistoryStoreError> {
        let raw = self.reader.get(IndexKey::manifest(k)).await?.ok_or_else(|| {
            HistoryStoreError::CorruptSegment {
                segment: k,
                file: "manifest".to_string(),
                reason: "missing".to_string(),
            }
        })?;
        SegmentManifest::decode(k, &raw)
    }

    /// Looks up a block number by hash, ignoring entries above `h`.
    pub async fn block_number(&self, hash: B256, h: u64) -> Result<Option<u64>, HistoryStoreError> {
        let Some(raw) = self.reader.get(IndexKey::block(hash)).await? else { return Ok(None) };
        let number = IndexKey::decode_block_value(&raw)?;
        Ok((number <= h).then_some(number))
    }

    /// Finds the first block `> block` and `≤ h` in which `key` changed (TDD §2.3: seek
    /// `h‖K‖segment(B)`, then the first block after B in that entry or the next entry).
    pub async fn first_change_after(
        &self,
        key: &HashedKey,
        block: u64,
        blocks_per_segment: u64,
        h: u64,
    ) -> Result<Option<u64>, HistoryStoreError> {
        let start = IndexKey::history(key, block / blocks_per_segment);
        let end = IndexKey::history(key, h / blocks_per_segment);
        let mut iter = self.reader.scan(start..=end).await?;
        while let Some(entry) = iter.next().await? {
            let segment = IndexKey::history_segment(&entry.key).ok_or_else(|| {
                HistoryStoreError::Index { reason: "history key without segment".to_string() }
            })?;
            let bitmap = IndexKey::decode_bitmap_value(&entry.value)?;
            let first = segment * blocks_per_segment;
            if let Some(found) =
                bitmap.iter().map(|offset| first + u64::from(offset)).find(|b| *b > block)
            {
                return Ok((found <= h).then_some(found));
            }
        }
        Ok(None)
    }

    /// Closes the reader.
    pub async fn close(&self) -> Result<(), HistoryStoreError> {
        Ok(self.reader.close().await?)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use object_store::{local::LocalFileSystem, memory::InMemory};
    use slatedb::{CloseReason, ErrorKind};

    use super::*;
    use crate::test_utils::ChainFixture;

    /// Test options with a short poll interval.
    fn options() -> IndexOptions {
        IndexOptions { gc_enabled: true, manifest_poll_interval: Duration::from_millis(20) }
    }

    /// An empty-file manifest for segment `k` of 4 blocks.
    fn manifest(k: u64) -> SegmentManifest {
        SegmentManifest {
            segment: k,
            first_block: k * 4,
            last_block: k * 4 + 3,
            epoch: 1,
            files: BTreeMap::new(),
            tip: None,
        }
    }

    /// The watermark and manifest land in the same batch as the segment's entries: a reader
    /// sees watermark 3 then 7, and `ensure_covers` refuses an `H` above its view.
    #[tokio::test]
    async fn watermark_tracks_published_segments() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let layout = ArchiveLayout::new("p");
        let fixture = ChainFixture::new(8, 1);
        let writer = IndexWriter::open(Arc::clone(&store), &layout, options()).await.unwrap();
        let reader = IndexReader::open(Arc::clone(&store), &layout, options()).await.unwrap();
        assert_eq!(reader.watermark().await.unwrap(), None, "empty index has no watermark");
        assert!(
            matches!(reader.ensure_covers(0).await, Err(HistoryStoreError::Unavailable { .. })),
            "an empty view covers nothing"
        );
        writer
            .write_segment(&SegmentIndex::from_blocks(0, 4, &fixture.blocks[0..4]), &manifest(0))
            .await
            .unwrap();
        let reader = IndexReader::open(Arc::clone(&store), &layout, options()).await.unwrap();
        assert_eq!(reader.watermark().await.unwrap(), Some(3), "watermark after segment 0");
        reader.ensure_covers(3).await.unwrap();
        let err = reader.ensure_covers(7).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            HistoryStoreError::Unavailable {
                reason: "index view at Some(3) trails archive H 7".to_string()
            }
            .to_string(),
            "a view behind H refuses"
        );
        assert_eq!(reader.manifest(0).await.unwrap(), manifest(0), "manifest 0 stored");
        assert!(
            matches!(
                reader.manifest(1).await,
                Err(HistoryStoreError::CorruptSegment { segment: 1, .. })
            ),
            "a missing manifest is corrupt"
        );
        writer
            .write_segment(&SegmentIndex::from_blocks(1, 4, &fixture.blocks[4..8]), &manifest(1))
            .await
            .unwrap();
        let reader = IndexReader::open(store, &layout, options()).await.unwrap();
        assert_eq!(reader.watermark().await.unwrap(), Some(7), "watermark after segment 1");
    }

    /// Flushed entries are visible to a reader; entries above H are ignored.
    #[tokio::test]
    async fn flushed_entries_visible_and_above_h_ignored() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let layout = ArchiveLayout::new("p");
        let fixture = ChainFixture::new(8, 2);
        let writer = IndexWriter::open(Arc::clone(&store), &layout, options()).await.unwrap();
        writer
            .write_segment(&SegmentIndex::from_blocks(0, 4, &fixture.blocks[0..4]), &manifest(0))
            .await
            .unwrap();
        writer
            .write_segment(&SegmentIndex::from_blocks(1, 4, &fixture.blocks[4..8]), &manifest(1))
            .await
            .unwrap();
        let reader = IndexReader::open(store, &layout, options()).await.unwrap();

        let tx = fixture.blocks[5].transactions[1].tx_hash();
        assert_eq!(reader.tx_location(tx, 7).await.unwrap(), Some((5, 1)), "tx 5/1 found");
        assert_eq!(reader.tx_location(tx, 3).await.unwrap(), None, "tx above H is ignored");
        assert_eq!(
            reader.block_number(fixture.blocks[6].hash, 7).await.unwrap(),
            Some(6),
            "block hash resolves"
        );
        assert_eq!(
            reader.block_number(B256::with_last_byte(1), 7).await.unwrap(),
            None,
            "unknown hash is absent"
        );

        // Storage slot (number % 3) changes in blocks 1..=7: slot 1 changes at 1, 4, 7.
        let slot =
            HashedKey::storage(fixture.contract, B256::from(alloy_primitives::U256::from(1)));
        assert_eq!(reader.first_change_after(&slot, 0, 4, 7).await.unwrap(), Some(1), "after 0");
        assert_eq!(reader.first_change_after(&slot, 1, 4, 7).await.unwrap(), Some(4), "after 1");
        assert_eq!(reader.first_change_after(&slot, 4, 4, 7).await.unwrap(), Some(7), "after 4");
        assert_eq!(reader.first_change_after(&slot, 4, 4, 6).await.unwrap(), None, "7 above H");
        assert_eq!(reader.first_change_after(&slot, 7, 4, 7).await.unwrap(), None, "none after 7");
        let untouched = HashedKey::account(Address::with_last_byte(0x77));
        assert_eq!(
            reader.first_change_after(&untouched, 0, 4, 7).await.unwrap(),
            None,
            "untouched key has no change"
        );
        reader.close().await.unwrap();
        writer.close().await.unwrap();
    }

    /// Deleted keys disappear for readers.
    #[tokio::test]
    async fn deletes_are_visible() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let layout = ArchiveLayout::new("");
        let fixture = ChainFixture::new(4, 1);
        let index = SegmentIndex::from_blocks(0, 4, &fixture.blocks);
        let writer = IndexWriter::open(Arc::clone(&store), &layout, options()).await.unwrap();
        writer.write_segment(&index, &manifest(0)).await.unwrap();
        writer.delete_keys(&index.keys()).await.unwrap();
        let reader = IndexReader::open(store, &layout, options()).await.unwrap();
        for (key, _) in index.puts() {
            assert_eq!(reader.reader.get(&key).await.unwrap(), None, "key {key:?} tombstoned");
        }
    }

    /// Opening a second writer fences the first (TDD §7 single writer).
    #[tokio::test]
    async fn second_writer_fences_first() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let layout = ArchiveLayout::new("");
        let fixture = ChainFixture::new(4, 1);
        let index = SegmentIndex::from_blocks(0, 4, &fixture.blocks);
        let first = IndexWriter::open(Arc::clone(&store), &layout, options()).await.unwrap();
        let second = IndexWriter::open(store, &layout, options()).await.unwrap();
        assert_eq!(
            (first.epoch(), second.epoch()),
            (1, 2),
            "each open claims the next SlateDB epoch"
        );
        let err = first.write_segment(&index, &manifest(0)).await.unwrap_err();
        assert!(
            matches!(
                &err,
                HistoryStoreError::Slate(e) if e.kind() == ErrorKind::Closed(CloseReason::Fenced)
            ),
            "first writer must be fenced, got {err}"
        );
    }

    /// The index works on `LocalFileSystem` with GC disabled.
    #[tokio::test]
    async fn local_filesystem_without_gc() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> =
            Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
        let layout = ArchiveLayout::new("");
        let fixture = ChainFixture::new(4, 1);
        let opts = IndexOptions { gc_enabled: false, ..options() };
        let writer = IndexWriter::open(Arc::clone(&store), &layout, opts).await.unwrap();
        writer
            .write_segment(&SegmentIndex::from_blocks(0, 4, &fixture.blocks), &manifest(0))
            .await
            .unwrap();
        let reader = IndexReader::open(store, &layout, opts).await.unwrap();
        assert_eq!(
            reader.block_number(fixture.blocks[2].hash, 3).await.unwrap(),
            Some(2),
            "LocalFileSystem index lookup"
        );
    }
}
