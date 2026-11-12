//! Orphan segment garbage collection (TDD §4).
//!
//! A crashed writer can leave uploaded objects for a segment above `H` that was never published.
//! They are invisible to readers. This job deletes them once they are older than a threshold
//! (1 h in production; configurable for tests). A live writer always rebuilds byte-identical
//! objects, so deleting an orphan only costs a re-upload.
//!
//! The scan is bounded to segments `H/N + 1 ..= H/N + ORPHAN_WINDOW`, one prefix listing each,
//! instead of listing the whole bucket. Orphans cannot exist anywhere else:
//! - A writer only uploads the segment after the `H` it read, `H_read/N + 1`, and `H` never
//!   decreases, so an upload lands at or below the current `H/N + 1`.
//! - A stale writer's upload of a segment `≤ H/N` lands under its own epoch directory, which no
//!   manifest references; it is left for the pruner's sweep of `seg/<k>/`.
//!
//! The window is wider than the one segment this argument needs, as margin. Segments at or
//! below `H/N` are never listed, so published data cannot be deleted.
//!
//! Object stores have no conditional DELETE, so an ownership check before a deletion can
//! always be outdated by the time the deletion lands. Deletions are therefore confined to
//! objects no current or future owner can publish: the job deletes only objects under epoch
//! directories strictly older than its own epoch (`seg/<k>/<epoch>/`, epoch < own) and never
//! the directory the segment's index manifest `m‖k` references. A newer owner uploads under its
//! own, newer epoch, so even a superseded job's last deletion cannot reach it. The job reads
//! `H` from catalog versions whose `writer_epoch` equals its own, all read after the claim that
//! opened the instance.

use std::time::Duration;

use chrono::{DateTime, Utc};
use object_store::{ObjectStoreExt, path::Path};

use crate::{CatalogStore, HistoryStoreError, IndexWriter, SegmentObjects};

/// Production minimum age of an orphan before deletion.
pub const DEFAULT_ORPHAN_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Number of segments above `H` the orphan scan lists.
pub const ORPHAN_WINDOW: u64 = 16;

/// Result of one GC pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Deleted object paths.
    pub deleted: Vec<Path>,
    /// Orphans kept because they are younger than the threshold.
    pub kept_young: Vec<Path>,
    /// Objects kept because they belong to this or a newer epoch, or to the epoch directory
    /// the segment's index manifest references.
    pub kept_live: Vec<Path>,
    /// Segments listed, inclusive.
    pub scanned: Option<(u64, u64)>,
}

/// Deletes orphaned segment objects above `H`.
#[derive(Debug, Clone)]
pub struct OrphanGc {
    /// Catalog access.
    pub catalog: CatalogStore,
    /// Segment objects.
    pub objects: SegmentObjects,
    /// Catalog writer epoch this job acts under.
    pub epoch: u64,
}

impl OrphanGc {
    /// Creates a GC acting under writer epoch `epoch`.
    pub const fn new(catalog: CatalogStore, objects: SegmentObjects, epoch: u64) -> Self {
        Self { catalog, objects, epoch }
    }

    /// Deletes orphans in the scan window with `last_modified ≤ now − min_age` that lie under
    /// an epoch older than this job's and not referenced by `index`'s manifest for the segment.
    pub async fn run(
        &self,
        index: &IndexWriter,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<GcReport, HistoryStoreError> {
        let catalog = self.catalog.load_required().await?.catalog;
        catalog.check_owner(self.epoch)?;
        let n = catalog.blocks_per_segment()?;
        let first = catalog.h.map_or(0, |h| h / n + 1);
        let last = first + ORPHAN_WINDOW - 1;
        let cutoff = now
            - chrono::Duration::from_std(min_age)
                .map_err(|err| HistoryStoreError::Catalog { reason: err.to_string() })?;
        let mut report = GcReport { scanned: Some((first, last)), ..Default::default() };
        for k in first..=last {
            let referenced = index.manifest(k).await?.map(|manifest| manifest.epoch);
            for meta in self.objects.list(k).await? {
                let Some((_, epoch)) = self.objects.layout.parse_object(&meta.location) else {
                    continue;
                };
                if epoch >= self.epoch || referenced == Some(epoch) {
                    report.kept_live.push(meta.location);
                    continue;
                }
                if meta.last_modified > cutoff {
                    report.kept_young.push(meta.location);
                    continue;
                }
                let latest = self.catalog.load_required().await?.catalog;
                latest.check_owner(self.epoch)?;
                if latest.h.is_some_and(|h| k <= h / n) {
                    break;
                }
                self.objects.store.delete(&meta.location).await?;
                report.deleted.push(meta.location);
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory};

    use super::*;
    use crate::{
        ArchiveLayout, PrepareOutcome, SegmentBuilder, StepOutcome,
        test_utils::{ChainFixture, GateOp, GatedStore, fixture_writer},
    };

    /// Sorted string forms of `paths`.
    fn sorted(paths: &[Path]) -> Vec<String> {
        let mut out: Vec<String> = paths.iter().map(ToString::to_string).collect();
        out.sort();
        out
    }

    /// Orphans of older epochs in the window above H are deleted only once older than the
    /// threshold; this epoch's objects are kept; segments at or below H are never listed.
    #[tokio::test]
    async fn deletes_only_old_orphans_above_h() {
        let fixture = ChainFixture::new(4, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        writer.step().await.unwrap();
        assert_eq!(writer.epoch, 1, "first writer holds epoch 1");
        let layout = ArchiveLayout::new("chain");
        for k in [1, 5, 17] {
            store.put(&layout.segment_object(k, 0, "headers.dat"), b"orphan".to_vec().into()).await.unwrap();
        }
        store.put(&layout.segment_object(2, 1, "headers.dat"), b"own".to_vec().into()).await.unwrap();
        // A stray object under published segment 0 must survive: segments <= H are not scanned.
        store.put(&layout.segment_object(0, 0, "stray.dat"), b"stray".to_vec().into()).await.unwrap();
        let gc = OrphanGc::new(writer.catalog.clone(), writer.objects.clone(), writer.epoch);

        let young = gc.run(&writer.index, DEFAULT_ORPHAN_MIN_AGE, Utc::now()).await.unwrap();
        assert_eq!(young.scanned, Some((1, 16)), "window is H/N+1 ..= H/N+16");
        assert_eq!(young.deleted.len(), 0, "young orphans are kept");
        assert_eq!(young.kept_young.len(), 2, "both in-window older-epoch orphans are young");

        let later = Utc::now() + chrono::Duration::hours(2);
        let old = gc.run(&writer.index, DEFAULT_ORPHAN_MIN_AGE, later).await.unwrap();
        assert_eq!(
            sorted(&old.deleted),
            vec!["chain/seg/1/0/headers.dat".to_string(), "chain/seg/5/0/headers.dat".to_string()],
            "exactly the two older-epoch orphans are deleted"
        );
        assert_eq!(sorted(&old.kept_live), vec!["chain/seg/2/1/headers.dat".to_string()], "own epoch kept");
        assert_eq!(writer.objects.list(0).await.unwrap().len(), 22, "segment 0 and its stray are untouched");
        assert_eq!(writer.objects.list(17).await.unwrap().len(), 1, "outside the window is not scanned");
    }

    /// A GC whose epoch was superseded deletes nothing.
    #[tokio::test]
    async fn superseded_gc_is_fenced() {
        let fixture = ChainFixture::new(4, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        writer.step().await.unwrap();
        let layout = ArchiveLayout::new("chain");
        store.put(&layout.segment_object(1, 0, "headers.dat"), b"orphan".to_vec().into()).await.unwrap();
        let gc = OrphanGc::new(writer.catalog.clone(), writer.objects.clone(), writer.epoch);
        writer.catalog.claim_at(writer.epoch + 1).await.unwrap();
        let err = gc
            .run(&writer.index, Duration::ZERO, Utc::now() + chrono::Duration::hours(2))
            .await
            .unwrap_err();
        assert!(matches!(err, HistoryStoreError::Fenced { .. }), "superseded GC is fenced, got {err}");
        assert_eq!(writer.objects.list(1).await.unwrap().len(), 1, "orphan not deleted");
    }

    /// A GC paused after its first ownership check, while a newer owner claims the archive,
    /// re-reads the catalog before its delete and stops `Fenced` without deleting anything.
    #[tokio::test]
    async fn gc_superseded_after_listing_deletes_nothing() {
        let fixture = ChainFixture::new(4, 1);
        let shared: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let gated = GatedStore::new(Arc::clone(&shared));
        let collector = fixture_writer(Arc::new(gated.clone()), &fixture, 4).await;
        collector.step().await.unwrap();
        let orphan = ArchiveLayout::new("chain").segment_object(1, 0, "headers.dat");
        shared.put(&orphan, b"orphan".to_vec().into()).await.unwrap();
        let mut gate = gated.arm(GateOp::List, "chain/seg/1");
        let gc = OrphanGc::new(collector.catalog.clone(), collector.objects.clone(), collector.epoch);
        let epoch = collector.epoch;
        let run = tokio::spawn(async move {
            gc.run(&collector.index, Duration::ZERO, Utc::now() + chrono::Duration::hours(2)).await
        });
        gate.reached().await;
        let owner = fixture_writer(Arc::clone(&shared), &fixture, 4).await;
        gate.release();
        let err = run.await.unwrap().expect_err("the superseded GC must stop");
        assert!(
            matches!(err, HistoryStoreError::Fenced { owned, current } if owned == epoch && current == owner.epoch),
            "fenced by the newer owner's claim, got {err}"
        );
        assert!(shared.head(&orphan).await.is_ok(), "the orphan was not deleted by the superseded GC");
    }

    /// Review2 P1: a GC that passed its ownership and `H` checks is paused at its DELETE while a
    /// newer owner rebuilds and publishes the same segment from byte-identical objects. When the
    /// stale DELETE lands it removes only the crashed epoch's object; the published segment
    /// still downloads and verifies.
    #[tokio::test]
    async fn stale_gc_delete_cannot_remove_newer_owners_publication() {
        let fixture = ChainFixture::new(8, 1);
        let shared: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let gated = GatedStore::new(Arc::clone(&shared));
        let crashed = fixture_writer(Arc::clone(&shared), &fixture, 4).await;
        crashed.step().await.unwrap();
        // The crashed writer left one object of segment 1, identical to any rebuild.
        let files = SegmentBuilder::new(4).build(1, &fixture.blocks[4..8]).unwrap();
        let orphan = ArchiveLayout::new("chain").segment_object(1, crashed.epoch, "headers.dat");
        shared.put(&orphan, files.files["headers.dat"].0.clone().into()).await.unwrap();
        drop(crashed);

        let collector = fixture_writer(Arc::new(gated.clone()), &fixture, 4).await;
        let mut gate = gated.arm(GateOp::Delete, "chain/seg/1/");
        let gc = OrphanGc::new(collector.catalog.clone(), collector.objects.clone(), collector.epoch);
        let run = tokio::spawn(async move {
            gc.run(&collector.index, Duration::ZERO, Utc::now() + chrono::Duration::hours(2)).await
        });
        assert_eq!(gate.reached().await, orphan, "GC pauses at the orphan's DELETE");

        let owner = fixture_writer(Arc::clone(&shared), &fixture, 4).await;
        assert!(
            matches!(owner.step().await.unwrap(), StepOutcome::Published { segment: 1, h: 7, .. }),
            "the newer owner publishes segment 1 while the GC is paused"
        );
        gate.release();
        // After its DELETE the fenced GC may stop on its next ownership or index check.
        match run.await.unwrap() {
            Ok(report) => assert_eq!(sorted(&report.deleted), vec![orphan.to_string()], "only the orphan is deleted"),
            Err(err) => assert!(
                matches!(err, HistoryStoreError::Fenced { .. } | HistoryStoreError::Slate(_)),
                "a stale GC can only stop fenced, got {err}"
            ),
        }
        assert!(
            matches!(shared.head(&orphan).await, Err(object_store::Error::NotFound { .. })),
            "the stale DELETE landed"
        );

        let manifest = owner.index.manifest(1).await.unwrap().expect("segment 1 manifest");
        assert_eq!(manifest.epoch, owner.epoch, "segment 1 is published from the owner's epoch");
        let published = owner.objects.download(&manifest, 4).await.expect("published objects survive");
        manifest.check(1, 4, &published.files).expect("published objects verify");
    }

    /// The epoch directory a segment's index manifest references is never deleted, even when it
    /// belongs to an older epoch and the segment is above `H`; unreferenced older epochs are.
    #[tokio::test]
    async fn keeps_epoch_dir_referenced_by_manifest() {
        let fixture = ChainFixture::new(8, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        old.step().await.unwrap();
        assert!(
            matches!(old.prepare().await.unwrap(), PrepareOutcome::Ready(_)),
            "segment 1 uploaded and indexed under epoch 1 but not published"
        );
        drop(old);
        let layout = ArchiveLayout::new("chain");
        store.put(&layout.segment_object(2, 1, "headers.dat"), b"orphan".to_vec().into()).await.unwrap();
        let writer = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        store.put(&layout.segment_object(2, writer.epoch, "headers.dat"), b"own".to_vec().into()).await.unwrap();
        let gc = OrphanGc::new(writer.catalog.clone(), writer.objects.clone(), writer.epoch);

        let report =
            gc.run(&writer.index, Duration::ZERO, Utc::now() + chrono::Duration::hours(2)).await.unwrap();
        assert_eq!(sorted(&report.deleted), vec!["chain/seg/2/1/headers.dat".to_string()], "unreferenced orphan deleted");
        assert_eq!(report.kept_live.len(), 22, "21 referenced objects of segment 1 plus the own-epoch object");
        assert_eq!(writer.objects.list(1).await.unwrap().len(), 21, "the referenced epoch directory is intact");
    }
}
