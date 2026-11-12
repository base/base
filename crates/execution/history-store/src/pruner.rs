//! Archive retention (TDD §6).
//!
//! Retirement is two-phase so every step is recoverable and readers that resolved an older `E`
//! keep working until the grace period ends:
//!
//! 1. **Retire** segment `k` (the segment at `E`): one catalog CAS that sets `E := (k+1)·N` and
//!    records `k` in `retired`. Nothing is deleted. Readers treat `n < E` as `HistoryPruned`
//!    regardless of `retired`; readers on an older snapshot still find the index entries and
//!    objects.
//! 2. **Sweep** `k` once the grace period (24 h in production) has passed:
//!    a. if the manifest `m‖k` is still present, recompute the segment's index keys from its
//!       objects and tombstone them together with `m‖k` in one atomic batch;
//!    b. delete `seg/<k>/*` under every epoch directory (idempotent);
//!    c. CAS the catalog to drop `k` from `retired`.
//!
//!    A missing `m‖k` means (a) already ran, because tombstones are flushed before any object
//!    is deleted. A crash after (a), during (b) or before (c) therefore converges on the next
//!    sweep. `retired` is bounded: entries leave it once swept.
//!
//! Every catalog change is conditional on the pruner's writer epoch, and each deletion first
//! re-reads the catalog to check ownership. That check and the deletion are not atomic: an
//! instance superseded between them can still delete objects of a segment that is already
//! below `E` and past its grace period, which only repeats work the new owner would do.

use std::time::Duration;

use tracing::info;

use crate::{
    Catalog, CatalogStore, HistoryStoreError, IndexKey, IndexWriter, RetiredSegment,
    SegmentIndex, SegmentObjects,
};

/// Production grace period between retirement and deletion.
pub const DEFAULT_RETIREMENT_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Default cap on segments retired, and on segments swept, per maintenance pass.
pub const DEFAULT_MAX_SEGMENTS_PER_PASS: usize = 4;

/// Archive retention job.
#[derive(Debug, Clone)]
pub struct Pruner {
    /// Catalog access.
    pub catalog: CatalogStore,
    /// Segment objects.
    pub objects: SegmentObjects,
    /// Catalog writer epoch this pruner acts under.
    pub epoch: u64,
    /// Most segments retired, and most swept, in one call.
    pub max_segments_per_pass: usize,
}

impl Pruner {
    /// Creates a pruner acting under writer epoch `epoch`.
    pub const fn new(
        catalog: CatalogStore,
        objects: SegmentObjects,
        epoch: u64,
        max_segments_per_pass: usize,
    ) -> Self {
        Self { catalog, objects, epoch, max_segments_per_pass }
    }

    /// Retires up to `max_segments_per_pass` segments whose last block's timestamp is older
    /// than `now − retention`, one at a time, never retiring the segment that ends at `H`.
    /// Returns the retired segments.
    pub async fn retire_older_than(
        &self,
        index: &IndexWriter,
        retention: Duration,
        now_ms: u64,
    ) -> Result<Vec<u64>, HistoryStoreError> {
        let cutoff_secs = now_ms.saturating_sub(retention.as_millis() as u64) / 1000;
        let mut retired = Vec::new();
        while retired.len() < self.max_segments_per_pass {
            let current = self.catalog.load_required().await?;
            current.catalog.check_owner(self.epoch)?;
            let n = current.catalog.blocks_per_segment()?;
            let k = current.catalog.e / n;
            let Some(h) = current.catalog.h else { break };
            if (k + 1) * n - 1 >= h {
                break;
            }
            let manifest = index.required_manifest(k).await?;
            let files = self.objects.download(&manifest, n).await?;
            manifest.check(k, n, &files.files)?;
            let blocks = files.read_blocks()?;
            let last_timestamp = blocks.last().map_or(u64::MAX, |block| block.header.timestamp);
            if last_timestamp > cutoff_secs {
                break;
            }
            self.retire(k, now_ms).await?;
            retired.push(k);
        }
        Ok(retired)
    }

    /// Retires segment `k`, which must be the segment at `E`: advances `E` past it and records
    /// it for the sweep. Deletes nothing.
    pub async fn retire(&self, k: u64, now_ms: u64) -> Result<(), HistoryStoreError> {
        let current = self.catalog.load_required().await?;
        let n = current.catalog.blocks_per_segment()?;
        if current.catalog.e != k * n {
            return Err(HistoryStoreError::Catalog {
                reason: format!("segment {k} is not at E {}", current.catalog.e),
            });
        }
        if current.catalog.h.is_none_or(|h| (k + 1) * n - 1 >= h) {
            return Err(HistoryStoreError::Catalog {
                reason: format!("segment {k} is not below the segment at H"),
            });
        }
        let mut retired = current.catalog.retired.clone();
        retired.push(RetiredSegment { segment: k, retired_at_ms: now_ms });
        let next = Catalog {
            generation: current.catalog.generation + 1,
            e: (k + 1) * n,
            retired,
            ..current.catalog.clone()
        };
        self.catalog.update_owned(&current, next, self.epoch).await?;
        info!(segment = k, "retired archive segment");
        Ok(())
    }

    /// Sweeps up to `max_segments_per_pass` retired segments whose grace period has passed:
    /// tombstone their index entries, delete their objects, then drop them from `retired`.
    /// Returns the swept segments.
    pub async fn sweep(
        &self,
        index: &IndexWriter,
        grace: Duration,
        now_ms: u64,
    ) -> Result<Vec<u64>, HistoryStoreError> {
        let current = self.catalog.load_required().await?;
        current.catalog.check_owner(self.epoch)?;
        let n = current.catalog.blocks_per_segment()?;
        let grace_ms = grace.as_millis() as u64;
        let expired: Vec<u64> = current
            .catalog
            .retired
            .iter()
            .filter(|retired| now_ms.saturating_sub(retired.retired_at_ms) >= grace_ms)
            .map(|retired| retired.segment)
            .take(self.max_segments_per_pass)
            .collect();
        if expired.is_empty() {
            return Ok(Vec::new());
        }
        for &k in &expired {
            let manifest_key = IndexKey::manifest(k);
            if let Some(manifest) = index.manifest(k).await? {
                let files = self.objects.download(&manifest, n).await?;
                manifest.check(k, n, &files.files)?;
                let blocks = files.read_blocks()?;
                let mut keys = SegmentIndex::from_blocks(k, n, &blocks).keys();
                keys.push(manifest_key);
                index.delete_keys(&keys).await?;
                info!(segment = k, tombstones = keys.len(), "tombstoned retired segment");
            }
            self.catalog.load_required().await?.catalog.check_owner(self.epoch)?;
            self.objects.delete(k).await?;
        }
        let mut current = current;
        loop {
            let next = Catalog {
                generation: current.catalog.generation + 1,
                retired: current
                    .catalog
                    .retired
                    .iter()
                    .filter(|retired| !expired.contains(&retired.segment))
                    .cloned()
                    .collect(),
                ..current.catalog.clone()
            };
            match self.catalog.update_owned(&current, next, self.epoch).await {
                Ok(_) => return Ok(expired),
                Err(HistoryStoreError::CasConflict) => {
                    current = self.catalog.load_required().await?;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory};

    use super::*;
    use crate::{
        ArchiveLayout, ArchiveWriter, IndexReader,
        test_utils::{ChainFixture, fixture_writer, test_index_options},
    };

    /// Publishes `segments` segments of 4 blocks and returns the writer and fixture.
    async fn published(segments: u64) -> (Arc<dyn ObjectStore>, ArchiveWriter, ChainFixture) {
        let fixture = ChainFixture::new(segments * 4, 2);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        for _ in 0..segments {
            writer.step().await.unwrap();
        }
        (store, writer, fixture)
    }

    /// A pruner acting under the writer's epoch.
    fn pruner(writer: &ArchiveWriter, cap: usize) -> Pruner {
        Pruner::new(writer.catalog.clone(), writer.objects.clone(), writer.epoch, cap)
    }

    /// Whether the index still holds segment 0's entries (block 2's hash and the manifest).
    async fn segment0_indexed(store: &Arc<dyn ObjectStore>, fixture: &ChainFixture) -> (bool, bool) {
        let reader =
            IndexReader::open(Arc::clone(store), &ArchiveLayout::new("chain"), test_index_options())
                .await
                .unwrap();
        (
            reader.block_number(fixture.blocks[2].hash, u64::MAX).await.unwrap().is_some(),
            reader.reader.get(IndexKey::manifest(0)).await.unwrap().is_some(),
        )
    }

    /// Retirement only advances E; tombstones and deletion wait for the sweep after grace.
    #[tokio::test]
    async fn retires_by_age_then_sweeps_after_grace() {
        let (store, writer, fixture) = published(3).await;
        let pruner = pruner(&writer, DEFAULT_MAX_SEGMENTS_PER_PASS);
        // Cut-off equals block 3's timestamp: only segment 0 is old enough.
        let now_ms = fixture.blocks[3].header.timestamp * 1000;
        let retired = pruner.retire_older_than(&writer.index, Duration::ZERO, now_ms).await.unwrap();
        assert_eq!(retired, vec![0], "only segment 0 is retired");

        let catalog = writer.catalog.load_required().await.unwrap().catalog;
        assert_eq!((catalog.e, catalog.h), (4, Some(11)), "E advanced to 4");
        assert_eq!(
            catalog.retired,
            vec![RetiredSegment { segment: 0, retired_at_ms: now_ms }],
            "retirement recorded"
        );
        assert_eq!(
            segment0_indexed(&store, &fixture).await,
            (true, true),
            "retirement alone keeps segment 0's entries for old snapshots"
        );

        // A stale upload under another epoch directory of segment 0 is swept too.
        let stale = ArchiveLayout::new("chain").segment_object(0, writer.epoch + 7, "headers.dat");
        store.put(&stale, b"stale".to_vec().into()).await.unwrap();
        let grace = Duration::from_secs(10);
        assert_eq!(
            pruner.sweep(&writer.index, grace, now_ms + 5_000).await.unwrap(),
            Vec::<u64>::new(),
            "grace not over"
        );
        assert_eq!(writer.objects.list(0).await.unwrap().len(), 22, "objects of both epochs kept during grace");
        assert_eq!(
            pruner.sweep(&writer.index, grace, now_ms + 10_000).await.unwrap(),
            vec![0],
            "grace over"
        );
        assert_eq!(writer.objects.list(0).await.unwrap().len(), 0, "objects of every epoch deleted after grace");
        assert_eq!(segment0_indexed(&store, &fixture).await, (false, false), "entries tombstoned");
        let reader =
            IndexReader::open(store, &ArchiveLayout::new("chain"), test_index_options()).await.unwrap();
        assert_eq!(
            reader.block_number(fixture.blocks[5].hash, 11).await.unwrap(),
            Some(5),
            "retained block hash still indexed"
        );
        let catalog = writer.catalog.load_required().await.unwrap().catalog;
        assert!(catalog.retired.is_empty(), "retired list cleared");
        assert_eq!(catalog.e, 4, "E unchanged by sweep");
    }

    /// A sweep interrupted after the tombstone batch, after a partial delete, or before the
    /// final CAS converges on rerun: objects gone, entries gone, `retired` empty.
    #[tokio::test]
    async fn sweep_converges_after_crash_at_each_step() {
        for crash in ["after_tombstone", "partial_delete", "before_cas"] {
            let (store, writer, fixture) = published(2).await;
            let pruner = pruner(&writer, DEFAULT_MAX_SEGMENTS_PER_PASS);
            pruner.retire(0, 0).await.unwrap();
            // Replay the sweep's steps by hand up to the crash point.
            let manifest = writer.index.required_manifest(0).await.unwrap();
            let blocks = writer.objects.download(&manifest, 4).await.unwrap().read_blocks().unwrap();
            let mut keys = SegmentIndex::from_blocks(0, 4, &blocks).keys();
            keys.push(IndexKey::manifest(0));
            writer.index.delete_keys(&keys).await.unwrap();
            match crash {
                "partial_delete" => {
                    for meta in writer.objects.list(0).await.unwrap().into_iter().take(7) {
                        store.delete(&meta.location).await.unwrap();
                    }
                }
                "before_cas" => {
                    writer.objects.delete(0).await.unwrap();
                }
                _ => {}
            }
            assert_eq!(
                pruner.sweep(&writer.index, Duration::ZERO, 1).await.unwrap(),
                vec![0],
                "{crash}: rerun sweeps segment 0"
            );
            assert_eq!(writer.objects.list(0).await.unwrap().len(), 0, "{crash}: objects deleted");
            assert_eq!(
                segment0_indexed(&store, &fixture).await,
                (false, false),
                "{crash}: entries tombstoned"
            );
            let catalog = writer.catalog.load_required().await.unwrap().catalog;
            assert!(catalog.retired.is_empty(), "{crash}: retired list cleared");
            assert_eq!(writer.objects.list(1).await.unwrap().len(), 21, "{crash}: segment 1 kept");
            assert_eq!(
                pruner.sweep(&writer.index, Duration::ZERO, 2).await.unwrap(),
                Vec::<u64>::new(),
                "{crash}: a second rerun has nothing left"
            );
        }
    }

    /// One pass retires at most `max_segments_per_pass` segments.
    #[tokio::test]
    async fn retirement_is_capped_per_pass() {
        let (_store, writer, _fixture) = published(5).await;
        let pruner = pruner(&writer, 2);
        assert_eq!(pruner.retire_older_than(&writer.index, Duration::ZERO, u64::MAX).await.unwrap(), vec![0, 1], "first pass");
        assert_eq!(writer.catalog.load_required().await.unwrap().catalog.e, 8, "E after first pass");
        assert_eq!(pruner.retire_older_than(&writer.index, Duration::ZERO, u64::MAX).await.unwrap(), vec![2, 3], "second pass");
        assert_eq!(
            pruner.retire_older_than(&writer.index, Duration::ZERO, u64::MAX).await.unwrap(),
            Vec::<u64>::new(),
            "segment 4 ends at H and stays"
        );
        assert_eq!(
            pruner.sweep(&writer.index, Duration::ZERO, u64::MAX).await.unwrap(),
            vec![0, 1],
            "sweep is capped too"
        );
    }

    /// A pruner whose epoch was superseded is fenced and changes nothing.
    #[tokio::test]
    async fn superseded_pruner_is_fenced() {
        let (_store, writer, _fixture) = published(3).await;
        let pruner = pruner(&writer, DEFAULT_MAX_SEGMENTS_PER_PASS);
        writer.catalog.claim_at(writer.epoch + 1).await.unwrap();
        let before = writer.catalog.load_required().await.unwrap();
        let err = pruner.retire_older_than(&writer.index, Duration::ZERO, u64::MAX).await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Fenced { owned, current } if owned == writer.epoch && current == writer.epoch + 1),
            "superseded pruner is fenced, got {err}"
        );
        assert_eq!(writer.catalog.load_required().await.unwrap(), before, "catalog unchanged");
    }

    /// The segment ending at H is never retired.
    #[tokio::test]
    async fn never_retires_segment_at_h() {
        let (_store, writer, _fixture) = published(1).await;
        let retired = pruner(&writer, 4).retire_older_than(&writer.index, Duration::ZERO, u64::MAX).await.unwrap();
        assert_eq!(retired, Vec::<u64>::new(), "segment at H stays");
        assert_eq!(writer.catalog.load_required().await.unwrap().catalog.e, 0, "E unchanged");
    }
}
