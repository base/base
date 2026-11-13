//! The stateless archive writer loop (TDD §4).
//!
//! Each step publishes at most one segment: read the catalog, fetch and verify the next
//! segment's blocks from the node list, confirm the last hash with a second fetch, build the
//! static files, upload them with `If-None-Match: *`, write the segment's index entries in one
//! batch and flush, then advance `H` by compare-and-swap.
//!
//! Fencing: opening a writer first opens `SlateDB`, which claims the next `SlateDB` writer epoch
//! and fences every older index writer, then CASes the catalog's `writer_epoch` to that same
//! number, only if it is greater than the stored one. One number therefore orders both claims:
//! whichever instance opened the index last owns the catalog too, and an instance that loses
//! either race fails to open or is fenced on both. Every catalog update the writer makes is
//! conditional on its own epoch still being current, so a writer that was paused anywhere
//! (even after its index flush) and then superseded can never move `H`. Segment objects are
//! uploaded under `seg/<k>/<epoch>/`, so a restarted writer never reuses an older epoch's
//! uploads and a stale garbage collector cannot delete objects this writer publishes.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use alloy_primitives::B256;
use object_store::ObjectStore;
use tracing::{debug, info, warn};

use crate::{
    ArchiveLayout, ArchiveReader, Catalog, CatalogStore, ChainTip, ChunkVerifier,
    HistoryStoreError, IndexOptions, IndexWriter, SegmentBuilder, SegmentIndex, SegmentManifest,
    SegmentObjects, SegmentTip, SourcePool, UploadReport, VersionedCatalog,
};

/// A segment uploaded and indexed but not yet published: the state between the index flush and
/// the catalog CAS.
#[derive(Debug, Clone)]
pub struct PreparedSegment {
    /// Catalog the segment was built against.
    pub current: VersionedCatalog,
    /// Segment number.
    pub segment: u64,
    /// Last block of the segment.
    pub last: u64,
    /// Chain tip at `last`, which the next segment chains onto.
    pub tip: ChainTip,
    /// Upload counts.
    pub upload: UploadReport,
}

/// Result of [`ArchiveWriter::prepare`].
#[derive(Debug, Clone)]
pub enum PrepareOutcome {
    /// The segment is ready to publish.
    Ready(Box<PreparedSegment>),
    /// Nothing to publish this step.
    Skip(StepOutcome),
}

/// Writer tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterConfig {
    /// Blocks requested per `archive_getBlockRange` call.
    pub chunk_blocks: u64,
    /// Sleep between steps when nothing new is finalized.
    pub idle_interval: Duration,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self { chunk_blocks: 1024, idle_interval: Duration::from_secs(2) }
    }
}

/// Chain identity the writer publishes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainIdentity {
    /// Chain id.
    pub chain_id: u64,
    /// Genesis block hash.
    pub genesis_hash: B256,
    /// Blocks per segment `N`.
    pub blocks_per_segment: u64,
}

/// Result of one writer step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// A segment was published and `H` advanced.
    Published {
        /// Segment number.
        segment: u64,
        /// New `H`.
        h: u64,
        /// Upload counts.
        upload: UploadReport,
    },
    /// The next segment is not finalized on any reachable node yet.
    NotReady {
        /// Last block of the next segment.
        needed: u64,
        /// Highest finalized block reported, if any node answered.
        finalized: Option<u64>,
    },
    /// Another writer published this segment first; this step's work was discarded.
    Discarded {
        /// Segment number.
        segment: u64,
    },
}

/// The archive writer.
#[derive(Debug)]
pub struct ArchiveWriter {
    /// Catalog access.
    pub catalog: CatalogStore,
    /// Segment objects.
    pub objects: SegmentObjects,
    /// Index writer (holds the `SlateDB` writer epoch).
    pub index: IndexWriter,
    /// Node list.
    pub sources: SourcePool,
    /// Payload verifier.
    pub verifier: ChunkVerifier,
    /// Tuning.
    pub config: WriterConfig,
    /// Catalog writer epoch this instance claimed.
    pub epoch: u64,
    /// Tip of the last segment this writer published, so the next `prepare` reads nothing back.
    pub last_tip: Mutex<Option<ChainTip>>,
}

impl ArchiveWriter {
    /// Initialises the catalog if absent, checks it matches `chain`, opens the index writer
    /// (fencing any previous writer) and claims the catalog under the index's writer epoch.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        layout: ArchiveLayout,
        chain: ChainIdentity,
        sources: SourcePool,
        verifier: ChunkVerifier,
        config: WriterConfig,
        index_options: IndexOptions,
    ) -> Result<Self, HistoryStoreError> {
        if config.chunk_blocks == 0 {
            return Err(HistoryStoreError::InvalidConfig {
                reason: "chunk_blocks must be at least 1".to_string(),
            });
        }
        if chain.blocks_per_segment == 0 {
            return Err(HistoryStoreError::InvalidConfig {
                reason: "blocks_per_segment must be at least 1".to_string(),
            });
        }
        let catalog = CatalogStore::new(Arc::clone(&store), layout.clone());
        catalog
            .init(&Catalog::new(chain.chain_id, chain.genesis_hash, chain.blocks_per_segment))
            .await?;
        let index = IndexWriter::open(Arc::clone(&store), &layout, index_options).await?;
        let epoch = index.epoch();
        if let Err(err) = catalog.claim_at(epoch).await {
            if let Err(close) = index.close().await {
                warn!(error = %close, "closing index after a lost ownership claim failed");
            }
            return Err(err);
        }
        Ok(Self {
            catalog,
            objects: SegmentObjects::new(store, layout),
            index,
            sources,
            verifier,
            config,
            epoch,
            last_tip: Mutex::new(None),
        })
    }

    /// Runs steps until an error. `Halt` and fencing errors end the loop; transient source
    /// errors are logged and retried after the idle interval.
    pub async fn run(&self) -> Result<(), HistoryStoreError> {
        loop {
            match self.step().await {
                Ok(StepOutcome::Published { segment, h, .. }) => {
                    info!(segment, h, "published archive segment");
                }
                Ok(StepOutcome::Discarded { segment }) => {
                    info!(segment, "segment already published by another writer");
                }
                Ok(StepOutcome::NotReady { .. }) => {
                    tokio::time::sleep(self.config.idle_interval).await
                }
                Err(err @ (HistoryStoreError::NoSource { .. } | HistoryStoreError::Source(_))) => {
                    warn!(error = %err, "writer stalled; retrying");
                    tokio::time::sleep(self.config.idle_interval).await;
                }
                Err(HistoryStoreError::SourceBusy { .. }) => {
                    debug!("every source is busy; backing off");
                    tokio::time::sleep(self.config.idle_interval).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Publishes the next segment if it is finalized.
    pub async fn step(&self) -> Result<StepOutcome, HistoryStoreError> {
        match self.prepare().await? {
            PrepareOutcome::Ready(prepared) => {
                let outcome = self
                    .publish(prepared.current, prepared.segment, prepared.last, prepared.upload)
                    .await?;
                if matches!(outcome, StepOutcome::Published { .. }) {
                    *self.last_tip.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(prepared.tip);
                }
                Ok(outcome)
            }
            PrepareOutcome::Skip(outcome) => Ok(outcome),
        }
    }

    /// Fetches, verifies, uploads and indexes the next segment if it is finalized, stopping
    /// just before the catalog CAS.
    pub async fn prepare(&self) -> Result<PrepareOutcome, HistoryStoreError> {
        let current = self.catalog.load_required().await?;
        current.catalog.check_owner(self.epoch)?;
        let n = current.catalog.blocks_per_segment()?;
        let segment = current.catalog.next_segment()?;
        let first = segment * n;
        let last = first + n - 1;

        let status = self.sources.status().await;
        let finalized = status.finalized;
        if let (Some(finalized), Some(h)) = (finalized, current.catalog.h)
            && finalized.saturating_sub(h) > 2 * n
        {
            warn!(finalized, h, lag = finalized - h, "archive lags finalized by more than 2N");
        }
        if finalized.is_none_or(|finalized| finalized < last) {
            return Ok(PrepareOutcome::Skip(StepOutcome::NotReady { needed: last, finalized }));
        }

        let mut tip = self.archive_tip(&current.catalog, segment, n).await?;
        if let Some(tip) = tip {
            // A finalized hash change under a published segment halts publication (TDD §7).
            let node_hash = self.sources.fetch_hash(tip.number).await?;
            if node_hash != tip.hash {
                return Err(HistoryStoreError::Halt {
                    block: tip.number,
                    archive: tip.hash,
                    node: node_hash,
                });
            }
        }

        // Never ask for more than every source advertises; halve on a too-large rejection.
        let mut chunk_blocks =
            self.config.chunk_blocks.min(status.max_range_blocks.unwrap_or(u64::MAX)).max(1);
        let mut blocks = Vec::with_capacity(n as usize);
        let mut from = first;
        while from <= last {
            let to = (from + chunk_blocks - 1).min(last);
            let chunk = match self.sources.fetch_verified(from, to, tip, &self.verifier).await {
                Ok(chunk) => chunk,
                Err(HistoryStoreError::SourceRangeTooLarge { .. }) if to > from => {
                    chunk_blocks = (to - from).div_ceil(2).max(1);
                    warn!(from, chunk_blocks, "range too large; halving chunk");
                    continue;
                }
                Err(HistoryStoreError::SourceRangeTooLarge { .. }) => {
                    return Err(HistoryStoreError::RangeTooLarge { block: from });
                }
                Err(err) => return Err(err),
            };
            tip = chunk.last().map(ChainTip::after);
            blocks.extend(chunk);
            from = to + 1;
        }
        if segment == 0 && blocks[0].hash != current.catalog.genesis_hash {
            return Err(HistoryStoreError::CatalogMismatch {
                field: "genesis_hash",
                expected: current.catalog.genesis_hash.to_string(),
                actual: blocks[0].hash.to_string(),
            });
        }

        let last_hash = blocks.last().expect("segment has blocks").hash;
        let confirmed = self.sources.fetch_hash(last).await?;
        if confirmed != last_hash {
            return Err(HistoryStoreError::Halt {
                block: last,
                archive: last_hash,
                node: confirmed,
            });
        }

        let tip = ChainTip::after(blocks.last().expect("segment has blocks"));
        let files = SegmentBuilder::new(n).build(segment, &blocks)?;
        // Uploads a crashed or superseded owner left for this segment are never published (the
        // segment is above `H` and this writer owns the catalog), and GC never revisits the
        // segment once it is published, so they are removed here. A superseded writer still
        // paused mid-upload can add objects after this; see the crate README.
        let superseded = self.objects.delete_older_epochs(segment, self.epoch).await?;
        if superseded > 0 {
            info!(segment, objects = superseded, "deleted superseded uploads");
        }
        let upload = self.objects.upload(&files, self.epoch).await?;
        self.index
            .write_segment(
                &SegmentIndex::from_blocks(segment, n, &blocks),
                &SegmentManifest {
                    tip: Some(SegmentTip { last_hash: tip.hash, next_tx_num: tip.next_tx_num }),
                    ..files.manifest(self.epoch)
                },
            )
            .await?;
        Ok(PrepareOutcome::Ready(Box::new(PreparedSegment { current, segment, last, tip, upload })))
    }

    /// Advances `H` to `last` by compare-and-swap, re-reading on conflict (TDD §4: on 412
    /// reread; if `H` advanced past the segment discard, else retry). Fails with
    /// [`HistoryStoreError::Fenced`] once another instance claimed the archive.
    pub async fn publish(
        &self,
        mut current: VersionedCatalog,
        segment: u64,
        last: u64,
        upload: UploadReport,
    ) -> Result<StepOutcome, HistoryStoreError> {
        loop {
            if current.catalog.h.is_some_and(|h| h >= last) {
                return Ok(StepOutcome::Discarded { segment });
            }
            let next = Catalog {
                generation: current.catalog.generation + 1,
                h: Some(last),
                ..current.catalog.clone()
            };
            match self.catalog.update_owned(&current, next, self.epoch).await {
                Ok(_) => return Ok(StepOutcome::Published { segment, h: last, upload }),
                Err(HistoryStoreError::CasConflict) => {
                    current = self.catalog.load_required().await?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Returns the hash and next transaction number of block `segment·N − 1`, or `None` for
    /// segment 0. Taken from the segment this writer last published, else from the previous
    /// segment's manifest; only a manifest written before tips were recorded makes the writer
    /// download and decode the previous segment, on the blocking pool.
    pub async fn archive_tip(
        &self,
        catalog: &Catalog,
        segment: u64,
        n: u64,
    ) -> Result<Option<ChainTip>, HistoryStoreError> {
        if segment == 0 {
            return Ok(None);
        }
        let previous = segment - 1;
        if catalog.h != Some(segment * n - 1) {
            return Err(HistoryStoreError::Catalog {
                reason: format!("H {:?} does not end segment {previous}", catalog.h),
            });
        }
        let cached = *self.last_tip.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tip) = cached.filter(|tip| tip.number == segment * n - 1) {
            return Ok(Some(tip));
        }
        let manifest = self.index.required_manifest(previous).await?;
        if let Some(tip) = manifest.chain_tip() {
            return Ok(Some(tip));
        }
        debug!(segment = previous, "manifest has no tip; reading the segment back");
        let files = self.objects.download(&manifest, n).await?;
        let blocks = ArchiveReader::blocking(move || {
            manifest.check(previous, n, &files.files)?;
            files.read_blocks()
        })
        .await?;
        Ok(blocks.last().map(ChainTip::after))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use alloy_primitives::Bytes;
    use alloy_rlp::Encodable;
    use object_store::{ObjectStoreExt, memory::InMemory};
    use slatedb::{CloseReason, ErrorKind};

    use super::*;
    use crate::{
        ArchiveBlock, ArchiveStatus, IndexReader, MockChainSource, test_utils::ChainFixture,
    };

    /// Blocks per segment used by writer tests.
    const N: u64 = 4;

    /// A mock node serving `fixture` with the given finalized block.
    fn node(fixture: &ChainFixture, finalized: u64) -> MockChainSource {
        crate::test_utils::mock_node(fixture, finalized)
    }

    /// A mock node whose range calls always fail.
    fn dead_node() -> MockChainSource {
        let mut source = MockChainSource::new();
        source.expect_name().return_const("dead".to_string());
        source.expect_status().returning(|| Err(HistoryStoreError::Source("down".to_string())));
        source
            .expect_get_block_range()
            .returning(|_, _| Err(HistoryStoreError::Source("down".to_string())));
        source
    }

    /// Returns `block` with a different header (and therefore hash).
    fn forked(block: &ArchiveBlock) -> ArchiveBlock {
        let mut header = block.decode().unwrap().header;
        header.extra_data = Bytes::from_static(b"fork");
        let mut raw = Vec::new();
        header.encode(&mut raw);
        ArchiveBlock { header: raw.into(), ..block.clone() }
    }

    /// Opens a writer over `store` with the given sources and chunk size.
    async fn writer(
        store: Arc<dyn ObjectStore>,
        fixture: &ChainFixture,
        sources: Vec<Arc<dyn crate::ChainSource>>,
        chunk_blocks: u64,
    ) -> ArchiveWriter {
        open_writer(store, fixture, sources, chunk_blocks).await.unwrap()
    }

    /// Opens a writer over `store`, returning the open error instead of panicking.
    async fn open_writer(
        store: Arc<dyn ObjectStore>,
        fixture: &ChainFixture,
        sources: Vec<Arc<dyn crate::ChainSource>>,
        chunk_blocks: u64,
    ) -> Result<ArchiveWriter, HistoryStoreError> {
        ArchiveWriter::open(
            store,
            ArchiveLayout::new("chain"),
            ChainIdentity {
                chain_id: 1,
                genesis_hash: fixture.blocks[0].hash,
                blocks_per_segment: N,
            },
            SourcePool::new(sources),
            ChunkVerifier::new(Arc::clone(&fixture.chain_spec)),
            WriterConfig { chunk_blocks, idle_interval: Duration::from_millis(10) },
            IndexOptions { gc_enabled: true, manifest_poll_interval: Duration::from_millis(20) },
        )
        .await
    }

    /// Collects every segment object's bytes.
    async fn objects(store: &Arc<dyn ObjectStore>) -> Vec<(String, Vec<u8>)> {
        let objects = SegmentObjects::new(Arc::clone(store), ArchiveLayout::new("chain"));
        let mut out = Vec::new();
        for (_, meta) in objects.list_all().await.unwrap() {
            let bytes = store.get(&meta.location).await.unwrap().bytes().await.unwrap();
            out.push((meta.location.to_string(), bytes.to_vec()));
        }
        out.sort();
        out
    }

    /// Segments publish in order, H advances by whole segments, and index entries resolve.
    #[tokio::test]
    async fn publishes_segments_in_order() {
        let fixture = ChainFixture::new(10, 2);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 9))], 3).await;

        let first = w.step().await.unwrap();
        assert_eq!(
            first,
            StepOutcome::Published {
                segment: 0,
                h: 3,
                upload: UploadReport { created: 21, already_present: 0 }
            },
            "segment 0 publishes all 21 objects"
        );
        let second = w.step().await.unwrap();
        assert!(
            matches!(second, StepOutcome::Published { segment: 1, h: 7, .. }),
            "segment 1 publishes, got {second:?}"
        );
        assert_eq!(
            w.step().await.unwrap(),
            StepOutcome::NotReady { needed: 11, finalized: Some(9) },
            "segment 2 needs block 11 finalized"
        );
        let catalog = w.catalog.load_required().await.unwrap().catalog;
        assert_eq!(
            (catalog.generation, catalog.h, catalog.e, catalog.writer_epoch),
            (3, Some(7), 0, 1),
            "catalog after the claim and two segments"
        );

        let reader = IndexReader::open(
            store,
            &ArchiveLayout::new("chain"),
            IndexOptions { gc_enabled: true, manifest_poll_interval: Duration::from_millis(20) },
        )
        .await
        .unwrap();
        assert_eq!(
            reader.block_number(fixture.blocks[6].hash, 7).await.unwrap(),
            Some(6),
            "block 6 hash indexed"
        );
        assert_eq!(
            reader.tx_location(fixture.blocks[7].transactions[1].tx_hash(), 7).await.unwrap(),
            Some((7, 1)),
            "tx 7/1 indexed"
        );
    }

    /// Every published segment's objects, read through the writer's own manifests and keyed by
    /// segment and file name, ignoring which epoch directory holds them.
    async fn published(w: &ArchiveWriter, segments: u64) -> Vec<(u64, String, Vec<u8>)> {
        let mut out = Vec::new();
        for k in 0..segments {
            let manifest =
                w.index.manifest(k).await.unwrap().expect("published segment has a manifest");
            for (name, bytes) in w.objects.download(&manifest, N).await.unwrap().files {
                out.push((k, name, bytes.to_vec()));
            }
        }
        out
    }

    /// Review5 P2 writer tip: after a publish, the next `prepare` chains onto the tip the
    /// writer kept, and a restarted writer onto the tip recorded in the previous segment's
    /// manifest; neither reads a segment object back.
    #[tokio::test]
    async fn prepare_after_publish_reads_no_segment_objects() {
        let fixture = ChainFixture::new(12, 2);
        let counting = crate::test_utils::ChunkedStore::new(
            Arc::new(InMemory::new()),
            "chain/seg/",
            usize::MAX,
        );
        let stats = Arc::clone(&counting.stats);
        let store: Arc<dyn ObjectStore> = Arc::new(counting);
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 11))], 4).await;
        w.step().await.unwrap();
        let tip = ChainTip::after(&fixture.blocks[3]);
        assert_eq!(*w.last_tip.lock().unwrap(), Some(tip), "the writer keeps segment 0's tip");
        assert_eq!(
            w.index.manifest(0).await.unwrap().and_then(|m| m.chain_tip()),
            Some(tip),
            "segment 0's manifest records its tip"
        );
        let outcome = w.step().await.unwrap();
        assert!(matches!(outcome, StepOutcome::Published { segment: 1, .. }), "got {outcome:?}");
        assert_eq!(stats.gets.load(Ordering::SeqCst), 0, "segment 1 chained without seg/ reads");
        drop(w);

        let restarted =
            writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 11))], 4).await;
        assert_eq!(*restarted.last_tip.lock().unwrap(), None, "a restart starts with no tip");
        let outcome = restarted.step().await.unwrap();
        assert!(matches!(outcome, StepOutcome::Published { segment: 2, .. }), "got {outcome:?}");
        assert_eq!(
            stats.gets.load(Ordering::SeqCst),
            0,
            "the restart chained onto segment 1's manifest tip without seg/ reads"
        );
    }

    /// A previous segment whose manifest predates recorded tips is read back to find the tip,
    /// and the next segment still publishes.
    #[tokio::test]
    async fn legacy_manifest_without_a_tip_reads_the_segment_back() {
        let fixture = ChainFixture::new(8, 2);
        let counting = crate::test_utils::ChunkedStore::new(
            Arc::new(InMemory::new()),
            "chain/seg/",
            usize::MAX,
        );
        let stats = Arc::clone(&counting.stats);
        let store: Arc<dyn ObjectStore> = Arc::new(counting);
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        w.step().await.unwrap();
        drop(w);
        let restarted =
            writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        let legacy = SegmentManifest {
            tip: None,
            ..restarted.index.manifest(0).await.unwrap().expect("segment 0 manifest")
        };
        restarted.index.db.put(crate::IndexKey::manifest(0), legacy.encode()).await.unwrap();
        restarted.index.db.flush().await.unwrap();
        let outcome = restarted.step().await.unwrap();
        assert!(matches!(outcome, StepOutcome::Published { segment: 1, h: 7, .. }), "{outcome:?}");
        assert_eq!(
            stats.gets.load(Ordering::SeqCst),
            21,
            "the 21 objects of segment 0 were read back once to find its tip"
        );
    }

    /// A crash after a partial upload is repaired by the next run, which deletes the crashed
    /// epoch's objects and uploads a complete set under its own epoch; the published bytes
    /// are identical to an uninterrupted run with another chunk size (TDD §8 mid-segment
    /// restart; paths differ by epoch, bytes do not).
    #[tokio::test]
    async fn restart_after_partial_upload_is_byte_identical() {
        let fixture = ChainFixture::new(8, 3);
        let reference: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w =
            writer(Arc::clone(&reference), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        w.step().await.unwrap();
        w.step().await.unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        // Simulate a crashed run under epoch 1: segment 0 partially uploaded, never published.
        let crashed =
            writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 1).await;
        assert_eq!(crashed.epoch, 1, "the crashed run held epoch 1");
        drop(crashed);
        let files = SegmentBuilder::new(N).build(0, &fixture.blocks[0..4]).unwrap();
        let objects_handle = SegmentObjects::new(Arc::clone(&store), ArchiveLayout::new("chain"));
        for (name, bytes) in files.files.iter().take(5) {
            store
                .put(
                    &ArchiveLayout::new("chain").segment_object(0, 1, name),
                    bytes.0.clone().into(),
                )
                .await
                .unwrap();
        }
        let restarted =
            writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 1).await;
        assert_eq!(restarted.epoch, 2, "the restart claims a newer epoch");
        let outcome = restarted.step().await.unwrap();
        assert_eq!(
            outcome,
            StepOutcome::Published {
                segment: 0,
                h: 3,
                upload: UploadReport { created: 21, already_present: 0 }
            },
            "restart uploads all 21 objects under its own epoch"
        );
        restarted.step().await.unwrap();
        assert_eq!(
            published(&restarted, 2).await,
            published(&w, 2).await,
            "published bytes are identical"
        );
        assert_eq!(
            restarted.index.manifest(0).await.unwrap().map(|manifest| manifest.epoch),
            Some(2),
            "segment 0 is published from the restart's epoch directory"
        );
        let remaining = objects_handle.list(0).await.unwrap();
        assert_eq!(remaining.len(), 21, "only the 21 published objects remain");
        assert!(
            remaining.iter().all(
                |meta| ArchiveLayout::new("chain").parse_object(&meta.location) == Some((0, 2))
            ),
            "no object of the crashed epoch remains: {remaining:?}"
        );
    }

    /// A different confirm hash for the segment's last block halts without publishing.
    #[tokio::test]
    async fn confirm_divergence_halts() {
        let fixture = ChainFixture::new(8, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut source = MockChainSource::new();
        source.expect_name().return_const("forky".to_string());
        source.expect_status().returning(|| {
            Ok(ArchiveStatus {
                head: 7,
                safe: 7,
                finalized: 7,
                earliest_local: 0,
                max_range_blocks: 10,
                max_range_bytes: 1 << 20,
            })
        });
        let served = fixture.clone();
        source.expect_get_block_range().returning(move |from, to| {
            if from == 3 && to == 3 {
                Ok(vec![forked(&served.wire(3, 3)[0])])
            } else {
                Ok(served.wire(from, to))
            }
        });
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(source)], 4).await;
        let err = w.step().await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Halt { block: 3, archive, .. } if archive == fixture.blocks[3].hash),
            "confirm mismatch must halt at block 3, got {err}"
        );
        let catalog = w.catalog.load_required().await.unwrap().catalog;
        assert_eq!(
            (catalog.generation, catalog.h),
            (1, None),
            "only the claim changed the catalog"
        );
        assert!(objects(&store).await.is_empty(), "nothing uploaded after halt");
    }

    /// A finalized hash change under a published segment halts the next step.
    #[tokio::test]
    async fn divergence_under_published_segment_halts() {
        let fixture = ChainFixture::new(8, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        w.step().await.unwrap();
        drop(w);

        let mut source = MockChainSource::new();
        source.expect_name().return_const("reorged".to_string());
        source.expect_status().returning(|| {
            Ok(ArchiveStatus {
                head: 7,
                safe: 7,
                finalized: 7,
                earliest_local: 0,
                max_range_blocks: 10,
                max_range_bytes: 1 << 20,
            })
        });
        let served = fixture.clone();
        source.expect_get_block_range().returning(move |from, to| {
            let mut rows = served.wire(from, to);
            if from == 3 {
                rows[0] = forked(&rows[0]);
            }
            Ok(rows)
        });
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(source)], 4).await;
        let err = w.step().await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Halt { block: 3, archive, .. } if archive == fixture.blocks[3].hash),
            "hash change under H must halt, got {err}"
        );
        assert_eq!(w.catalog.load_required().await.unwrap().catalog.h, Some(3), "H unchanged");
    }

    /// A dead node and a node with a bad payload are skipped; a good node completes the segment.
    #[tokio::test]
    async fn skips_dead_and_bad_nodes() {
        let fixture = ChainFixture::new(4, 2);
        let mut bad = MockChainSource::new();
        bad.expect_name().return_const("bad".to_string());
        bad.expect_status().returning(|| {
            Ok(ArchiveStatus {
                head: 3,
                safe: 3,
                finalized: 3,
                earliest_local: 0,
                max_range_blocks: 10,
                max_range_bytes: 1 << 20,
            })
        });
        let served = fixture.clone();
        bad.expect_get_block_range().returning(move |from, to| {
            let mut rows = served.wire(from, to);
            for row in &mut rows {
                row.senders.reverse();
            }
            Ok(rows)
        });
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(
            Arc::clone(&store),
            &fixture,
            vec![Arc::new(dead_node()), Arc::new(bad), Arc::new(node(&fixture, 3))],
            4,
        )
        .await;
        let outcome = w.step().await.unwrap();
        assert!(
            matches!(outcome, StepOutcome::Published { segment: 0, h: 3, .. }),
            "good node publishes segment 0, got {outcome:?}"
        );
    }

    /// With only bad nodes the writer stalls with `NoSource` and uploads nothing.
    #[tokio::test]
    async fn no_good_node_stalls() {
        let fixture = ChainFixture::new(4, 2);
        let mut bad = MockChainSource::new();
        bad.expect_name().return_const("bad".to_string());
        bad.expect_status().returning(|| {
            Ok(ArchiveStatus {
                head: 3,
                safe: 3,
                finalized: 3,
                earliest_local: 0,
                max_range_blocks: 10,
                max_range_bytes: 1 << 20,
            })
        });
        let served = fixture.clone();
        bad.expect_get_block_range().returning(move |from, to| {
            let mut rows = served.wire(from, to);
            rows[1].transactions.clear();
            Ok(rows)
        });
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(bad), Arc::new(dead_node())], 4)
            .await;
        let err = w.step().await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::NoSource { from: 0, to: 3 }),
            "all nodes failing must stall, got {err}"
        );
        assert!(objects(&store).await.is_empty(), "nothing uploaded");
    }

    /// A mock node that answers `archive_status` but rejects every range as busy (`4008`).
    fn busy_node() -> MockChainSource {
        let mut source = MockChainSource::new();
        source.expect_name().return_const("busy".to_string());
        source.expect_status().returning(|| {
            Ok(ArchiveStatus {
                head: 3,
                safe: 3,
                finalized: 3,
                earliest_local: 0,
                max_range_blocks: 10,
                max_range_bytes: 1 << 20,
            })
        });
        source.expect_get_block_range().returning(|_, _| {
            Err(HistoryStoreError::SourceBusy {
                source_name: "busy".to_string(),
                reason: "2 ranges in flight".to_string(),
            })
        });
        source
    }

    /// A busy node is skipped like an unavailable one and the next node serves the segment.
    #[tokio::test]
    async fn busy_node_is_skipped_for_the_next_source() {
        let fixture = ChainFixture::new(4, 2);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(
            Arc::clone(&store),
            &fixture,
            vec![Arc::new(busy_node()), Arc::new(node(&fixture, 3))],
            4,
        )
        .await;
        let outcome = w.step().await.unwrap();
        assert!(
            matches!(outcome, StepOutcome::Published { segment: 0, h: 3, .. }),
            "the second node publishes segment 0 past the busy one, got {outcome:?}"
        );
    }

    /// When every node is busy the writer reports `SourceBusy` (back off), not `NoSource`, and
    /// uploads nothing.
    #[tokio::test]
    async fn all_busy_nodes_back_off() {
        let fixture = ChainFixture::new(4, 2);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(
            Arc::clone(&store),
            &fixture,
            vec![Arc::new(busy_node()), Arc::new(busy_node())],
            4,
        )
        .await;
        let err = w.step().await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::SourceBusy { .. }),
            "only busy nodes must back off, got {err}"
        );
        assert!(objects(&store).await.is_empty(), "nothing uploaded");
    }

    /// A second writer fences the first; a stale catalog CAS discards instead of regressing.
    #[tokio::test]
    async fn duplicate_writer_is_fenced_and_stale_publish_discards() {
        let fixture = ChainFixture::new(8, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        let new = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        let err = old.step().await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Fenced { owned: 1, current: 2 }),
            "old writer must be fenced by the catalog epoch, got {err}"
        );
        let err = old
            .index
            .write_segment(
                &SegmentIndex::default(),
                &crate::SegmentManifest {
                    segment: 0,
                    first_block: 0,
                    last_block: 3,
                    epoch: 1,
                    files: Default::default(),
                    tip: None,
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HistoryStoreError::Slate(e) if e.kind() == ErrorKind::Closed(CloseReason::Fenced)),
            "old writer's index is fenced by SlateDB too, got {err}"
        );
        let stale = new.catalog.load_required().await.unwrap();
        assert!(
            matches!(new.step().await.unwrap(), StepOutcome::Published { segment: 0, .. }),
            "new writer publishes"
        );
        assert_eq!(
            new.publish(stale, 0, 3, UploadReport::default()).await.unwrap(),
            StepOutcome::Discarded { segment: 0 },
            "a publish from a stale catalog whose H already advanced is discarded"
        );
        let catalog = new.catalog.load_required().await.unwrap().catalog;
        assert_eq!((catalog.generation, catalog.h), (3, Some(3)), "two claims and one publication");
    }

    /// A writer paused between its index flush and its catalog CAS, then superseded, gets
    /// `Fenced` and leaves the catalog exactly as the new owner left it.
    #[tokio::test]
    async fn writer_paused_after_flush_is_fenced() {
        let fixture = ChainFixture::new(8, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        let PrepareOutcome::Ready(prepared) = old.prepare().await.unwrap() else {
            panic!("segment 0 is ready");
        };
        let _new = writer(Arc::clone(&store), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        let before = old.catalog.load_required().await.unwrap();
        let err = old
            .publish(prepared.current, prepared.segment, prepared.last, prepared.upload)
            .await
            .unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Fenced { owned: 1, current: 2 }),
            "paused writer is fenced, got {err}"
        );
        assert_eq!(old.catalog.load_required().await.unwrap(), before, "catalog unchanged");
        assert_eq!(before.catalog.h, None, "H never moved");
    }

    /// Runs one step on each writer and returns how many published.
    async fn functional(writers: [&ArchiveWriter; 2]) -> usize {
        let mut published = 0;
        for w in writers {
            if matches!(w.step().await, Ok(StepOutcome::Published { .. })) {
                published += 1;
            }
        }
        published
    }

    /// Two writers opening concurrently, with A paused before it touches the index: B opens
    /// completely, then A resumes. Ownership is the `SlateDB` epoch, so whichever instance opened
    /// the index last also owns the catalog, and exactly one writer can publish.
    #[tokio::test]
    async fn concurrent_open_paused_before_index_leaves_one_owner() {
        let fixture = ChainFixture::new(8, 1);
        let shared: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let gated = crate::test_utils::GatedStore::new(Arc::clone(&shared));
        let mut gate = gated.arm(crate::test_utils::GateOp::Any, "chain/idx/");
        let served = fixture.clone();
        let a = tokio::spawn(async move {
            let source: Arc<dyn crate::ChainSource> = Arc::new(node(&served, 7));
            open_writer(Arc::new(gated), &served, vec![source], 4).await
        });
        gate.reached().await;
        let b = writer(Arc::clone(&shared), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        gate.release();
        let a = a.await.unwrap().expect("A opens after B: it holds the newest epoch");
        assert_eq!((b.epoch, a.epoch), (1, 2), "A opened the index last");
        assert_eq!(functional([&b, &a]).await, 1, "exactly one writer can publish");
        let catalog = a.catalog.load_required().await.unwrap().catalog;
        assert_eq!(
            (catalog.writer_epoch, catalog.h),
            (2, Some(3)),
            "A owns the catalog and published"
        );
    }

    /// Two writers opening concurrently, with A paused after opening the index but before its
    /// catalog claim: B opens completely, then A's claim finds a newer epoch and A fails to open.
    #[tokio::test]
    async fn concurrent_open_paused_before_claim_leaves_one_owner() {
        let fixture = ChainFixture::new(8, 1);
        let shared: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let gated = crate::test_utils::GatedStore::new(Arc::clone(&shared));
        // The first catalog write is `init`; the second is the claim.
        let mut gate = gated.arm_after(crate::test_utils::GateOp::Put, "chain/catalog.json", 1);
        let served = fixture.clone();
        let a = tokio::spawn(async move {
            let source: Arc<dyn crate::ChainSource> = Arc::new(node(&served, 7));
            open_writer(Arc::new(gated), &served, vec![source], 4).await
        });
        gate.reached().await;
        let b = writer(Arc::clone(&shared), &fixture, vec![Arc::new(node(&fixture, 7))], 4).await;
        gate.release();
        let err = a.await.unwrap().expect_err("A's claim is older than B's");
        assert!(
            matches!(err, HistoryStoreError::Fenced { owned: 1, current: 2 }),
            "A is fenced at open, got {err}"
        );
        assert_eq!(b.epoch, 2, "B opened the index after A");
        assert!(
            matches!(b.step().await.unwrap(), StepOutcome::Published { segment: 0, .. }),
            "B publishes"
        );
    }

    /// `chunk_blocks = 0` and `N = 0` are rejected when the writer opens.
    #[tokio::test]
    async fn zero_chunk_and_segment_are_rejected() {
        let fixture = ChainFixture::new(4, 1);
        let open = |chunk_blocks, blocks_per_segment| {
            let fixture = fixture.clone();
            async move {
                ArchiveWriter::open(
                    Arc::new(InMemory::new()),
                    ArchiveLayout::new("chain"),
                    ChainIdentity {
                        chain_id: 1,
                        genesis_hash: fixture.blocks[0].hash,
                        blocks_per_segment,
                    },
                    SourcePool::new(Vec::new()),
                    ChunkVerifier::new(Arc::clone(&fixture.chain_spec)),
                    WriterConfig { chunk_blocks, idle_interval: Duration::from_millis(10) },
                    IndexOptions::default(),
                )
                .await
                .unwrap_err()
                .to_string()
            }
        };
        assert_eq!(
            open(0, 4).await,
            "invalid archive configuration: chunk_blocks must be at least 1",
            "zero chunk"
        );
        assert_eq!(
            open(4, 0).await,
            "invalid archive configuration: blocks_per_segment must be at least 1",
            "zero segment"
        );
    }

    /// Block ranges a recording node was asked for, in call order.
    type RecordedRanges = Arc<std::sync::Mutex<Vec<(u64, u64)>>>;

    /// A node that records every range it is asked for, advertises `max_range_blocks`, and
    /// rejects ranges longer than `accept` blocks as too large.
    fn recording_node(
        fixture: &ChainFixture,
        max_range_blocks: u64,
        accept: u64,
    ) -> (MockChainSource, RecordedRanges) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut source = MockChainSource::new();
        source.expect_name().return_const("recording".to_string());
        let tip = fixture.tip();
        source.expect_status().returning(move || {
            Ok(ArchiveStatus {
                head: tip,
                safe: tip,
                finalized: tip,
                earliest_local: 0,
                max_range_blocks,
                max_range_bytes: 1 << 20,
            })
        });
        let served = fixture.clone();
        let seen = Arc::clone(&calls);
        source.expect_get_block_range().returning(move |from, to| {
            seen.lock().unwrap().push((from, to));
            if to - from + 1 > accept {
                return Err(HistoryStoreError::SourceRangeTooLarge {
                    source_name: "recording".to_string(),
                    from,
                    to,
                    reason: "4004".to_string(),
                });
            }
            Ok(served.wire(from, to))
        });
        (source, calls)
    }

    /// The chunk is clamped to the smallest advertised `maxRangeBlocks`.
    #[tokio::test]
    async fn chunk_is_clamped_to_advertised_limit() {
        let fixture = ChainFixture::new(4, 1);
        let (source, calls) = recording_node(&fixture, 2, u64::MAX);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(store, &fixture, vec![Arc::new(source)], 1024).await;
        assert!(
            matches!(w.step().await.unwrap(), StepOutcome::Published { segment: 0, .. }),
            "published"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(0, 1), (2, 3), (3, 3)],
            "two 2-block chunks then the 1-block confirm"
        );
    }

    /// A too-large rejection halves the chunk until the source accepts it.
    #[tokio::test]
    async fn too_large_range_halves_chunk() {
        let fixture = ChainFixture::new(4, 1);
        let (source, calls) = recording_node(&fixture, 100, 1);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(store, &fixture, vec![Arc::new(source)], 4).await;
        assert!(
            matches!(w.step().await.unwrap(), StepOutcome::Published { segment: 0, .. }),
            "published"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(0, 3), (0, 1), (0, 0), (1, 1), (2, 2), (3, 3), (3, 3)],
            "4 → 2 → 1 blocks, then 1-block chunks and the confirm"
        );
    }

    /// A block too large even alone stops with a typed error instead of looping.
    #[tokio::test]
    async fn single_block_too_large_is_terminal() {
        let fixture = ChainFixture::new(4, 1);
        let (source, calls) = recording_node(&fixture, 100, 0);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let w = writer(Arc::clone(&store), &fixture, vec![Arc::new(source)], 4).await;
        let err = w.step().await.unwrap_err();
        assert!(matches!(err, HistoryStoreError::RangeTooLarge { block: 0 }), "got {err}");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(0, 3), (0, 1), (0, 0)],
            "halves down to one block, once"
        );
        assert!(objects(&store).await.is_empty(), "nothing uploaded");
    }

    /// A malformed confirm answer from one node moves on to the next node.
    #[tokio::test]
    async fn malformed_confirm_tries_next_source() {
        let fixture = ChainFixture::new(4, 1);
        let mut garbled = MockChainSource::new();
        garbled.expect_name().return_const("garbled".to_string());
        garbled.expect_status().returning(|| {
            Ok(ArchiveStatus {
                head: 3,
                safe: 3,
                finalized: 3,
                earliest_local: 0,
                max_range_blocks: 10,
                max_range_bytes: 1 << 20,
            })
        });
        let served = fixture.clone();
        garbled.expect_get_block_range().returning(move |from, to| {
            let mut rows = served.wire(from, to);
            if from == to {
                rows[0].header = Bytes::from_static(b"not rlp");
            }
            Ok(rows)
        });
        let pool = SourcePool::new(vec![Arc::new(garbled), Arc::new(node(&fixture, 3))]);
        assert_eq!(
            pool.fetch_hash(3).await.unwrap(),
            fixture.blocks[3].hash,
            "second node answers"
        );
    }

    /// Review2 P2 confirm: a node that answers the confirm fetch for block 3 with block 2's
    /// header, labelled as block 3, is skipped; the next node's hash is used.
    #[tokio::test]
    async fn confirm_with_another_blocks_header_tries_next_source() {
        let fixture = ChainFixture::new(4, 1);
        let mut relabelled = MockChainSource::new();
        relabelled.expect_name().return_const("relabelled".to_string());
        relabelled.expect_status().returning(|| {
            Ok(ArchiveStatus {
                head: 3,
                safe: 3,
                finalized: 3,
                earliest_local: 0,
                max_range_blocks: 10,
                max_range_bytes: 1 << 20,
            })
        });
        let served = fixture.clone();
        relabelled.expect_get_block_range().times(1).returning(move |from, to| {
            let mut rows = served.wire(from, to);
            if from == to && from > 0 {
                rows[0] =
                    ArchiveBlock { number: from, ..served.wire(from - 1, from - 1).remove(0) };
            }
            Ok(rows)
        });
        let pool = SourcePool::new(vec![Arc::new(relabelled), Arc::new(node(&fixture, 3))]);
        assert_eq!(
            pool.fetch_hash(3).await.unwrap(),
            fixture.blocks[3].hash,
            "the second node's hash is used"
        );
    }
}
