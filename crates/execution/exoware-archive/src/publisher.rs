//! CAS-free segment publication.
//!
//! Rows are a pure function of finalized block data, so any number of exporters may publish
//! the same segment: they write identical bytes. A segment becomes visible only when its
//! marker is written, after every row is acknowledged and after marker `k − 1` exists.
//! Marker checks before and during publication are diagnostics, not fencing: concurrent
//! *incompatible* writers are unsupported without conditional writes.

use std::{future::Future, ops::Range, sync::Arc, time::{Duration, Instant}};

use alloy_primitives::hex;
use bytes::Bytes;
use exoware_sdk::limits::{MAX_REQUEST_MESSAGE_BYTES, MAX_VALUE_LEN};
use tracing::{debug, error, info, warn};

use crate::{
    ArchiveConfig, ArchiveError, ArchiveKey, ArchiveMetrics, ArchiveStore, BlockSource,
    BlockVerifier, DEFAULT_PRUNE_GRACE_SEGMENTS, ExowareArchive, SegmentLoader, SegmentMarker,
    SegmentRows,
};

/// Default target size of one `Put` (16 MiB).
pub const DEFAULT_MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;

/// Default maximum rows in one `Put`.
pub const DEFAULT_MAX_BATCH_ROWS: usize = 65_536;

/// Publisher tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublisherOptions {
    /// Target encoded bytes per `Put`; a larger row goes in its own `Put`.
    pub max_batch_bytes: usize,
    /// Maximum rows per `Put`.
    pub max_batch_rows: usize,
    /// Attempts per `Put` before the segment is abandoned for this round.
    pub max_put_attempts: usize,
    /// First retry delay.
    pub initial_backoff: Duration,
    /// Largest retry delay.
    pub max_backoff: Duration,
    /// Segments kept below the earliest served segment before physical pruning (≥ 1).
    pub prune_grace_segments: u64,
    /// Delay between rounds when there is nothing to publish.
    pub idle_interval: Duration,
    /// Interval between reconcile prunes in [`SegmentPublisher::run_until`].
    pub reconcile_interval: Duration,
}

impl Default for PublisherOptions {
    fn default() -> Self {
        Self {
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
            max_batch_rows: DEFAULT_MAX_BATCH_ROWS,
            max_put_attempts: 5,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(10),
            prune_grace_segments: DEFAULT_PRUNE_GRACE_SEGMENTS,
            idle_interval: Duration::from_secs(2),
            reconcile_interval: Duration::from_secs(600),
        }
    }
}

impl PublisherOptions {
    /// Checks value ranges.
    pub fn validate(&self) -> Result<(), ArchiveError> {
        let invalid = |reason: &str| Err(ArchiveError::InvalidConfig { reason: reason.to_string() });
        if self.max_batch_bytes == 0 || self.max_batch_bytes > MAX_REQUEST_MESSAGE_BYTES {
            return invalid("max_batch_bytes must be in 1..=256 MiB");
        }
        if self.max_batch_rows == 0 {
            return invalid("max_batch_rows must be positive");
        }
        if self.max_put_attempts == 0 {
            return invalid("max_put_attempts must be positive");
        }
        if self.prune_grace_segments == 0 {
            return invalid("prune_grace_segments must be at least 1");
        }
        Ok(())
    }

    /// Splits rows into `Put`s of at most `max_batch_rows` rows and `max_batch_bytes` encoded
    /// bytes. A row larger than the byte target gets its own `Put`; a value above the store's
    /// 32 MiB limit is a terminal error for the segment.
    pub fn plan_chunks(
        &self,
        rows: &SegmentRows,
        prefix_len: usize,
    ) -> Result<Vec<Range<usize>>, ArchiveError> {
        let mut chunks = Vec::new();
        let mut start = 0;
        let mut bytes = 0;
        for (i, (key, value)) in rows.rows.iter().enumerate() {
            if value.len() > MAX_VALUE_LEN {
                return Err(ArchiveError::ValueTooLarge {
                    segment: rows.segment,
                    key: hex::encode(key),
                    len: value.len(),
                    max: MAX_VALUE_LEN,
                });
            }
            let entry = PutSize::entry(prefix_len + key.len(), value.len());
            let full = i > start
                && (i - start >= self.max_batch_rows
                    || bytes + entry > self.max_batch_bytes);
            if full {
                chunks.push(start..i);
                start = i;
                bytes = 0;
            }
            bytes += entry;
            if entry > self.max_batch_bytes {
                chunks.push(start..i + 1);
                start = i + 1;
                bytes = 0;
            }
        }
        if start < rows.rows.len() {
            chunks.push(start..rows.rows.len());
        }
        Ok(chunks)
    }

    /// Delay before retry number `attempt` (1-based).
    pub fn backoff(&self, attempt: usize) -> Duration {
        let factor = 1u32 << attempt.saturating_sub(1).min(16);
        self.initial_backoff.saturating_mul(factor).min(self.max_backoff)
    }
}

/// Result of publishing one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// This publisher wrote the marker.
    Published {
        /// Data `Put`s issued, including replays.
        puts: usize,
        /// Rows in the segment.
        rows: u64,
    },
    /// An identical marker already existed (another exporter, or a lost acknowledgement).
    AlreadyPublished,
}

/// Result of one exporter round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// A segment was published or found already published.
    Advanced {
        /// Segment number.
        segment: u64,
        /// How it was advanced.
        outcome: PublishOutcome,
    },
    /// The next segment is not finalized yet.
    Waiting {
        /// Next segment.
        segment: u64,
        /// Node's finalized block.
        finalized: Option<u64>,
    },
}

/// What marker `k` holds compared to the locally built one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerState {
    /// No marker.
    Absent,
    /// Identical marker.
    Equal,
}

/// Publishes segments from a [`BlockSource`] into an archive.
#[derive(Debug)]
pub struct SegmentPublisher<S, B: ?Sized> {
    /// Archive handle.
    pub archive: ExowareArchive<S>,
    /// Local block data.
    pub source: Arc<B>,
    /// Verifies blocks before they are written.
    pub verifier: BlockVerifier,
    /// Expected config; the stored row must equal it.
    pub config: ArchiveConfig,
    /// Tuning.
    pub options: PublisherOptions,
}

impl<S: ArchiveStore, B: BlockSource + ?Sized> SegmentPublisher<S, B> {
    /// Opens a publisher. The config row must already be provisioned and equal `config`.
    pub async fn open(
        archive: ExowareArchive<S>,
        source: Arc<B>,
        verifier: BlockVerifier,
        config: ArchiveConfig,
        options: PublisherOptions,
    ) -> Result<Self, ArchiveError> {
        config.validate()?;
        options.validate()?;
        archive.require_config(&config).await?;
        Ok(Self { archive, source, verifier, config, options })
    }

    /// Next segment to publish: highest marker + 1, or the start segment.
    pub async fn next_segment(&self) -> Result<u64, ArchiveError> {
        let session = self.archive.store.session();
        let highest = ExowareArchive::<S>::highest_marker(&session).await?;
        if let Some(highest) = highest {
            ArchiveMetrics::highest_marker().set(highest.segment as f64);
        }
        Ok(highest.map_or(self.config.start_segment, |h| h.segment.saturating_add(1)))
    }

    /// Loads, verifies and builds the rows of `segment` on the blocking pool.
    pub async fn build_segment(&self, segment: u64) -> Result<SegmentRows, ArchiveError>
    where
        B: 'static,
    {
        let source = Arc::clone(&self.source);
        let verifier = self.verifier.clone();
        let config = self.config;
        tokio::task::spawn_blocking(move || {
            let blocks = SegmentLoader::load(source.as_ref(), &config, segment)?;
            for block in &blocks {
                verifier.verify_block(block).map_err(|e| ArchiveError::InvalidBlock {
                    block: block.number(),
                    reason: e.to_string(),
                })?;
            }
            SegmentRows::build(&config, segment, &blocks)
        })
        .await
        .map_err(|e| ArchiveError::Local { reason: format!("segment build task failed: {e}") })?
    }

    /// Publishes `segment` from the local source.
    pub async fn publish_segment(&self, segment: u64) -> Result<PublishOutcome, ArchiveError>
    where
        B: 'static,
    {
        self.require_predecessor(segment).await?;
        let rows = self.build_segment(segment).await?;
        self.publish_rows(&rows).await
    }

    /// Publishes prebuilt rows: checks marker `k`, writes every chunk (replaying identical
    /// bytes after ambiguous failures), rechecks marker `k`, requires marker `k − 1`, then
    /// writes marker `k`.
    pub async fn publish_rows(&self, rows: &SegmentRows) -> Result<PublishOutcome, ArchiveError> {
        let segment = rows.segment;
        if self.marker_state(segment, &rows.marker).await? == MarkerState::Equal {
            return Ok(PublishOutcome::AlreadyPublished);
        }
        let chunks = self.options.plan_chunks(rows, self.archive.store.key_prefix_len())?;
        let mut puts = 0;
        for chunk in &chunks {
            match self.put_with_recovery(segment, &rows.marker, &rows.rows[chunk.clone()]).await? {
                (attempts, MarkerState::Absent) => puts += attempts,
                (_, MarkerState::Equal) => return Ok(PublishOutcome::AlreadyPublished),
            }
        }
        if self.marker_state(segment, &rows.marker).await? == MarkerState::Equal {
            return Ok(PublishOutcome::AlreadyPublished);
        }
        self.require_predecessor(segment).await?;
        let marker_row = [(ArchiveKey::marker(segment), Bytes::from(rows.marker.encode()))];
        if self.put_with_recovery(segment, &rows.marker, &marker_row).await?.1 == MarkerState::Equal {
            return Ok(PublishOutcome::AlreadyPublished);
        }
        ArchiveMetrics::segments_published().increment(1);
        ArchiveMetrics::highest_marker().set(segment as f64);
        info!(segment, rows = rows.marker.row_count, puts, digest = %rows.marker.digest, "published segment");
        Ok(PublishOutcome::Published { puts, rows: rows.marker.row_count })
    }

    /// Fails with [`ArchiveError::Contiguity`] unless marker `segment − 1` exists or `segment`
    /// is the start segment.
    pub async fn require_predecessor(&self, segment: u64) -> Result<(), ArchiveError> {
        if segment <= self.config.start_segment {
            return Ok(());
        }
        let session = self.archive.store.session();
        if ExowareArchive::<S>::marker(&session, segment - 1).await?.is_none() {
            return Err(ArchiveError::Contiguity { segment, missing: segment - 1 });
        }
        Ok(())
    }

    /// Compares marker `segment` with the local one. A different marker quarantines export.
    pub async fn marker_state(
        &self,
        segment: u64,
        local: &SegmentMarker,
    ) -> Result<MarkerState, ArchiveError> {
        let session = self.archive.store.session();
        match ExowareArchive::<S>::marker(&session, segment).await? {
            None => Ok(MarkerState::Absent),
            Some(stored) if stored == *local => Ok(MarkerState::Equal),
            Some(stored) => {
                ArchiveMetrics::quarantined().set(1.0);
                error!(
                    segment,
                    stored_digest = %stored.digest,
                    local_digest = %local.digest,
                    "stored segment marker differs from local rows; export quarantined"
                );
                Err(ArchiveError::Quarantined { segment, stored: stored.digest, local: local.digest })
            }
        }
    }

    /// Writes one chunk. After an ambiguous failure it backs off and rereads marker `segment`:
    /// an equal marker means the segment is complete; an absent one replays the same rows.
    /// Returns the number of `Put`s issued and the marker state that ended the loop.
    pub async fn put_with_recovery(
        &self,
        segment: u64,
        marker: &SegmentMarker,
        chunk: &[(Bytes, Bytes)],
    ) -> Result<(usize, MarkerState), ArchiveError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let started = Instant::now();
            match self.archive.store.put(chunk).await {
                Ok(sequence) => {
                    ArchiveMetrics::put_latency().record(started.elapsed().as_secs_f64());
                    debug!(segment, rows = chunk.len(), sequence, "put acknowledged");
                    return Ok((attempt, MarkerState::Absent));
                }
                Err(err) if attempt >= self.options.max_put_attempts => {
                    warn!(segment, attempt, error = %err, "put failed; giving up for this round");
                    return Err(err);
                }
                Err(err) => {
                    ArchiveMetrics::put_retries().increment(1);
                    warn!(segment, attempt, error = %err, "put outcome unknown; rechecking marker");
                    tokio::time::sleep(self.options.backoff(attempt)).await;
                    match self.marker_state(segment, marker).await {
                        Ok(MarkerState::Equal) => return Ok((attempt, MarkerState::Equal)),
                        Ok(MarkerState::Absent) => {}
                        Err(err @ ArchiveError::Quarantined { .. }) => return Err(err),
                        Err(err) => warn!(segment, error = %err, "marker recheck failed"),
                    }
                }
            }
        }
    }

    /// Publishes the next segment if it is finalized, then prunes.
    pub async fn step(&self) -> Result<StepOutcome, ArchiveError>
    where
        B: 'static,
    {
        let segment = self.next_segment().await?;
        let source = Arc::clone(&self.source);
        let finalized = tokio::task::spawn_blocking(move || source.finalized_block_number())
            .await
            .map_err(|e| ArchiveError::Local { reason: format!("finalized lookup failed: {e}") })??;
        let highest_block = segment.checked_sub(1).map(|s| self.config.last_block(s));
        if let (Some(finalized), Some(highest)) = (finalized, highest_block) {
            ArchiveMetrics::lag_blocks().set(finalized.saturating_sub(highest) as f64);
        }
        if finalized.is_none_or(|finalized| finalized < self.config.last_block(segment)) {
            return Ok(StepOutcome::Waiting { segment, finalized });
        }
        let outcome = self.publish_segment(segment).await?;
        self.archive.prune(&self.config, Some(segment), self.options.prune_grace_segments).await?;
        Ok(StepOutcome::Advanced { segment, outcome })
    }

    /// Prunes against the current highest marker. A stalled exporter can reinsert rows
    /// below the cutoff, so the run loop repeats this periodically.
    pub async fn reconcile_prune(&self) -> Result<Option<u64>, ArchiveError> {
        let session = self.archive.store.session();
        let highest = ExowareArchive::<S>::highest_marker(&session).await?.map(|h| h.segment);
        self.archive.prune(&self.config, highest, self.options.prune_grace_segments).await
    }

    /// Runs [`Self::step`] until `shutdown` resolves, backing off exponentially on errors.
    /// Quarantine and config errors stop the loop.
    pub async fn run_until(&self, shutdown: impl Future<Output = ()>) -> Result<(), ArchiveError>
    where
        B: 'static,
    {
        tokio::pin!(shutdown);
        let mut failures = 0;
        let mut last_reconcile = Instant::now();
        loop {
            if last_reconcile.elapsed() >= self.options.reconcile_interval {
                last_reconcile = Instant::now();
                if let Err(err) = self.reconcile_prune().await {
                    warn!(error = %err, "reconcile prune failed");
                }
            }
            let delay = match self.step().await {
                Ok(StepOutcome::Advanced { .. }) => {
                    failures = 0;
                    Duration::ZERO
                }
                Ok(StepOutcome::Waiting { .. }) => {
                    failures = 0;
                    self.options.idle_interval
                }
                Err(err @ (ArchiveError::Quarantined { .. }
                | ArchiveError::ConfigMismatch { .. }
                | ArchiveError::ConfigMissing
                | ArchiveError::ValueTooLarge { .. })) => {
                    error!(error = %err, "archive export halted");
                    return Err(err);
                }
                Err(err) => {
                    failures += 1;
                    warn!(error = %err, failures, "archive export round failed");
                    self.options.backoff(failures)
                }
            };
            tokio::select! {
                () = &mut shutdown => return Ok(()),
                () = tokio::time::sleep(delay) => {}
            }
        }
    }
}

/// Protobuf `PutRequest` size accounting by length only.
#[derive(Debug, Clone, Copy, Default)]
pub struct PutSize;

impl PutSize {
    /// Length of a protobuf varint.
    pub const fn varint_len(mut value: usize) -> usize {
        let mut len = 1;
        while value >= 0x80 {
            value >>= 7;
            len += 1;
        }
        len
    }

    /// Encoded size of one entry with a physical key of `key_len` bytes.
    pub const fn entry(key_len: usize, value_len: usize) -> usize {
        let key = if key_len == 0 { 0 } else { 1 + Self::varint_len(key_len) + key_len };
        let value = if value_len == 0 { 0 } else { 1 + Self::varint_len(value_len) + value_len };
        let entry = key + value;
        1 + Self::varint_len(entry) + entry
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;
    use crate::{
        ArchiveReader, ArchiveStore, BlockCodec, ExowareStore, Family, LocalAvailability,
        ProvisionOutcome, ReaderLimits, StoreSession,
        test_utils::{
            ChainFixture, FaultKind, FaultyStore, FixtureNode, PutFault, PutMatch, TEST_N,
            TEST_TIMEOUT, TestSimulator, dump, fast_options, publish_through, publisher, reader,
        },
    };

    /// Runs `fut` under the per-test network budget.
    async fn bounded<F: Future>(fut: F) -> F::Output {
        tokio::time::timeout(TEST_TIMEOUT, fut).await.expect("test timed out")
    }

    /// Publishes every segment of a clean store, for byte comparisons.
    async fn clean_dump(sim: &TestSimulator, namespace: &str, fixture: &Arc<ChainFixture>, last: u64) -> Vec<(Bytes, Bytes)> {
        let config = fixture.config(TEST_N, 0, None);
        let archive = sim.archive(namespace, &config).await;
        publish_through(&archive, fixture, config, last).await;
        dump(archive.store.as_ref()).await
    }

    /// Test 3: published segments read back equal to the fixture in every family, and the
    /// exporter waits at the first unfinalized segment.
    #[tokio::test(flavor = "multi_thread")]
    async fn publish_then_read_every_family() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t3/", &config).await;
            let mut node = FixtureNode::new(Arc::clone(&fixture));
            node.finalized = 4 * TEST_N - 1;
            let publisher = publisher(archive.clone(), node.clone(), config).await;
            for segment in 0..4 {
                let step = publisher.step().await.unwrap();
                assert_eq!(
                    step,
                    StepOutcome::Advanced {
                        segment,
                        outcome: PublishOutcome::Published {
                            puts: 1,
                            rows: SegmentRows::build(&config, segment, &fixture.blocks[(segment * TEST_N) as usize..((segment + 1) * TEST_N) as usize]).unwrap().marker.row_count,
                        },
                    },
                    "segment {segment} published in one put"
                );
            }
            assert_eq!(
                publisher.step().await.unwrap(),
                StepOutcome::Waiting { segment: 4, finalized: Some(31) },
                "segment 4 waits for finality"
            );

            let reader = reader(archive.clone(), node).await;
            let session = archive.store.session();
            for block in &fixture.blocks[..32] {
                let n = block.number();
                let archived = reader.block(n).await.unwrap();
                assert_eq!(archived.hash, block.hash(), "hash {n}");
                assert_eq!(archived.header, block.header, "header {n}");
                assert_eq!(archived.body, block.body, "body {n}");
                assert_eq!(archived.receipts, block.receipts, "receipts {n}");
                assert_eq!(archived.senders, block.senders, "senders {n}");
                assert_eq!(reader.block_number_by_hash(block.hash()).await.unwrap(), Some(n), "block locator {n}");
                for (index, tx) in block.body.transactions.iter().enumerate() {
                    let found = reader.transaction_by_hash(tx.tx_hash()).await.unwrap().expect("tx found");
                    assert_eq!((found.block_number, found.index, &found.transaction), (n, index as u32, tx), "tx locator {n}/{index}");
                    assert_eq!(found.block_hash, block.hash(), "tx block hash {n}/{index}");
                }
                let accounts = BlockCodec::decode_account_changes(&session.get(&ArchiveKey::block(Family::AccountChanges, n)).await.unwrap().expect("row")).unwrap();
                let mut expected = block.account_changes.clone();
                expected.sort_by_key(|c| c.address);
                assert_eq!(accounts, expected, "account changes {n}");
                let storage = BlockCodec::decode_storage_changes(&session.get(&ArchiveKey::block(Family::StorageChanges, n)).await.unwrap().expect("row")).unwrap();
                assert_eq!(storage.len(), block.storage_changes.len(), "storage changes {n}");
            }
            assert_eq!(
                reader.block(32).await.unwrap_err(),
                ArchiveError::NotArchived { block: 32, highest: Some(31) },
                "block above H is not archived"
            );
            assert_eq!(reader.block_number_by_hash(B256::repeat_byte(9)).await.unwrap(), None, "unknown hash");
            assert_eq!(reader.transaction_by_hash(fixture.blocks[33].body.transactions[1].tx_hash()).await.unwrap(), None, "unarchived tx");
        })
        .await;
    }

    /// Test 4 / A1: two publishers over independently built sources race on the same
    /// segments; the store ends with exactly one marker per segment and the same logical
    /// contents as a single-publisher store.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_publishers_converge() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture_a = Arc::new(ChainFixture::standard());
            let fixture_b = Arc::new(ChainFixture::standard());
            let config = fixture_a.config(TEST_N, 0, None);
            let shared = sim.archive("t4-shared/", &config).await;
            let a = publisher(shared.clone(), FixtureNode::new(Arc::clone(&fixture_a)), config).await;
            let b = publisher(shared.clone(), FixtureNode::new(Arc::clone(&fixture_b)), config).await;
            let run = |p: SegmentPublisher<ExowareStore, FixtureNode>| async move {
                let mut outcomes = Vec::new();
                for _ in 0..5 {
                    match p.step().await.expect("step succeeds") {
                        StepOutcome::Advanced { segment, outcome } => outcomes.push((segment, outcome)),
                        StepOutcome::Waiting { .. } => break,
                    }
                }
                outcomes
            };
            let (from_a, from_b) = tokio::join!(run(a), run(b));
            let published = from_a
                .iter()
                .chain(&from_b)
                .filter(|(_, o)| matches!(o, PublishOutcome::Published { .. }))
                .count();
            assert!(published >= 5, "every segment published at least once, got {from_a:?} {from_b:?}");

            let rows = dump(shared.store.as_ref()).await;
            let markers: Vec<_> = rows.iter().filter(|(k, _)| ArchiveKey::family(k) == Some(Family::Marker)).collect();
            assert_eq!(markers.len(), 5, "exactly one marker per segment");
            for (segment, (key, value)) in markers.iter().enumerate() {
                let built = SegmentRows::build(&config, segment as u64, &fixture_a.blocks[segment * 8..segment * 8 + 8]).unwrap();
                assert_eq!(key, &&ArchiveKey::marker(segment as u64), "marker key {segment}");
                assert_eq!(SegmentMarker::decode(value).unwrap(), built.marker, "marker {segment} digest");
            }
            let single = clean_dump(&sim, "t4-single/", &fixture_a, 4).await;
            assert_eq!(rows.len(), single.len(), "same row count as a single publisher");
            assert!(rows == single, "shared store equals the single-publisher store byte for byte");
        })
        .await;
    }

    /// Test 5: a failure after the first chunk leaves the segment invisible; a rerun
    /// completes with identical bytes.
    #[tokio::test(flavor = "multi_thread")]
    async fn failure_after_first_chunk_is_invisible() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let plain = sim.archive("t5/", &config).await;
            let faulty = Arc::new(FaultyStore::new(
                Arc::clone(&plain.store),
                vec![PutFault { when: PutMatch::AtOrAfter(1), kind: FaultKind::Fail, times: 5 }],
            ));
            let archive = ExowareArchive::new(Arc::clone(&faulty));
            let verifier = fixture.verifier();
            let options = PublisherOptions { max_batch_rows: 50, ..fast_options() };
            let node = FixtureNode::new(Arc::clone(&fixture));
            let p = SegmentPublisher::open(archive.clone(), Arc::new(node.clone()), verifier, config, options).await.unwrap();
            let err = p.publish_segment(0).await.unwrap_err();
            assert_eq!(err, ArchiveError::store("put", "injected failure"), "fifth failed attempt surfaces");
            assert_eq!(faulty.put_count(), 6, "one acked chunk plus five failed attempts");
            assert_eq!(p.next_segment().await.unwrap(), 0, "H unchanged");
            let r = reader(plain.clone(), node).await;
            assert_eq!(r.block(0).await.unwrap_err(), ArchiveError::NotArchived { block: 0, highest: None }, "segment invisible");
            let partial = dump(plain.store.as_ref()).await;
            assert!(partial.len() > 1, "first chunk was written");
            assert!(partial.iter().all(|(k, _)| ArchiveKey::family(k) != Some(Family::Marker)), "no marker");

            let outcome = p.publish_segment(0).await.unwrap();
            assert!(matches!(outcome, PublishOutcome::Published { .. }), "rerun publishes, got {outcome:?}");
            for segment in 1..5 {
                p.publish_segment(segment).await.unwrap();
            }
            assert!(dump(plain.store.as_ref()).await == clean_dump(&sim, "t5-clean/", &fixture, 4).await, "identical bytes");
        })
        .await;
    }

    /// A5: a lost acknowledgement on a data chunk replays the chunk; on the marker the
    /// recheck finds it equal. Both end with the clean store's logical contents.
    #[tokio::test(flavor = "multi_thread")]
    async fn lost_ack_recovery() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let clean = clean_dump(&sim, "t5a-clean/", &fixture, 1).await;
            for (namespace, fault, expected_first) in [
                ("t5a-data/", PutFault { when: PutMatch::AtOrAfter(1), kind: FaultKind::LostAck, times: 1 }, "data"),
                ("t5a-marker/", PutFault { when: PutMatch::Marker, kind: FaultKind::LostAck, times: 1 }, "marker"),
            ] {
                let plain = sim.archive(namespace, &config).await;
                let faulty = Arc::new(FaultyStore::new(Arc::clone(&plain.store), vec![fault]));
                let options = PublisherOptions { max_batch_rows: 50, ..fast_options() };
                let p = SegmentPublisher::open(
                    ExowareArchive::new(Arc::clone(&faulty)),
                    Arc::new(FixtureNode::new(Arc::clone(&fixture))),
                    fixture.verifier(),
                    config,
                    options,
                )
                .await
                .unwrap();
                let first = p.publish_segment(0).await.unwrap();
                match expected_first {
                    "data" => assert!(
                        matches!(first, PublishOutcome::Published { puts, .. } if puts == p.options.plan_chunks(&p.build_segment(0).await.unwrap(), faulty.key_prefix_len()).unwrap().len() + 1),
                        "data lost ack replays one chunk, got {first:?}"
                    ),
                    _ => assert_eq!(first, PublishOutcome::AlreadyPublished, "marker lost ack resolves by recheck"),
                }
                p.publish_segment(1).await.unwrap();
                assert!(dump(plain.store.as_ref()).await == clean, "{namespace} equals the clean store");
            }
        })
        .await;
    }

    /// Test 6: marker `k` is refused while marker `k − 1` is missing, before and after rows
    /// are written.
    #[tokio::test(flavor = "multi_thread")]
    async fn contiguity_is_enforced() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t6/", &config).await;
            let p = publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            assert_eq!(p.publish_segment(2).await.unwrap_err(), ArchiveError::Contiguity { segment: 2, missing: 1 }, "early check");
            assert_eq!(dump(archive.store.as_ref()).await.len(), 1, "only the config row exists");
            p.publish_segment(0).await.unwrap();
            let rows = p.build_segment(2).await.unwrap();
            assert_eq!(p.publish_rows(&rows).await.unwrap_err(), ArchiveError::Contiguity { segment: 2, missing: 1 }, "pre-marker check");
            let session = archive.store.session();
            assert_eq!(ExowareArchive::<ExowareStore>::marker(&session, 2).await.unwrap(), None, "marker 2 not written");
            assert_eq!(p.next_segment().await.unwrap(), 1, "H stays at segment 0");
        })
        .await;
    }

    /// Test 7 / A1: exporters require a provisioned, byte-identical config; readers disable
    /// the link for another chain.
    #[tokio::test(flavor = "multi_thread")]
    async fn config_mismatch_refuses() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let node = Arc::new(FixtureNode::new(Arc::clone(&fixture)));
            let open = |archive: ExowareArchive<ExowareStore>, config: ArchiveConfig| {
                SegmentPublisher::open(archive, Arc::clone(&node), fixture.verifier(), config, fast_options())
            };
            let empty = ExowareArchive::new(sim.store("t7-empty/"));
            assert_eq!(open(empty, config).await.unwrap_err(), ArchiveError::ConfigMissing, "no implicit config");
            let archive = sim.archive("t7/", &config).await;
            assert_eq!(archive.provision(&config).await.unwrap(), ProvisionOutcome::AlreadyProvisioned, "idempotent provision");
            let variants = [
                ArchiveConfig { format: 2, ..config },
                ArchiveConfig { blocks_per_segment: 16, ..config },
                ArchiveConfig { chain_id: 1, ..config },
                ArchiveConfig { genesis_hash: B256::repeat_byte(1), ..config },
            ];
            for variant in variants {
                assert!(
                    matches!(open(archive.clone(), variant).await.unwrap_err(), ArchiveError::ConfigMismatch { .. }),
                    "exporter refuses {variant}"
                );
                assert!(matches!(archive.provision(&variant).await.unwrap_err(), ArchiveError::ConfigMismatch { .. }), "provision refuses {variant}");
            }
            let open_reader = |chain_id: u64, genesis: B256| {
                ArchiveReader::open(archive.clone(), fixture.verifier(), Arc::clone(&node), chain_id, genesis, ReaderLimits::default())
            };
            assert!(matches!(open_reader(1, config.genesis_hash).await.unwrap_err(), ArchiveError::LinkDisabled { .. }), "chain id mismatch");
            assert!(matches!(open_reader(config.chain_id, B256::ZERO).await.unwrap_err(), ArchiveError::LinkDisabled { .. }), "genesis mismatch");
            assert!(open_reader(config.chain_id, config.genesis_hash).await.is_ok(), "matching identity opens");
        })
        .await;
    }

    /// A1: a stored marker that differs from the local rows quarantines export without
    /// writing any row.
    #[tokio::test(flavor = "multi_thread")]
    async fn different_marker_quarantines() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-quarantine/", &config).await;
            let bogus = SegmentMarker { last_hash: B256::ZERO, row_count: 1, digest: B256::repeat_byte(0xbb) };
            archive.store.put(&[(ArchiveKey::marker(0), Bytes::from(bogus.encode()))]).await.unwrap();
            let p = publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            let local = p.build_segment(0).await.unwrap().marker;
            assert_eq!(
                p.publish_segment(0).await.unwrap_err(),
                ArchiveError::Quarantined { segment: 0, stored: bogus.digest, local: local.digest },
                "export quarantined"
            );
            assert_eq!(dump(archive.store.as_ref()).await.len(), 2, "only config and the bogus marker");
        })
        .await;
    }

    /// A8: missing local data stops export; pruned senders are recovered with identical rows.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_availability_and_sender_recovery() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-local/", &config).await;
            let mut node = FixtureNode::new(Arc::clone(&fixture));
            node.availability = LocalAvailability { receipts: 5, ..LocalAvailability::default() };
            let p = publisher(archive.clone(), node, config).await;
            assert_eq!(
                p.publish_segment(0).await.unwrap_err(),
                ArchiveError::LocalDataPruned { table: "receipts", block: 0, earliest: 5 },
                "pruned receipts stop export"
            );
            let mut node = FixtureNode::new(Arc::clone(&fixture));
            node.senders_pruned = true;
            let recovered = publisher(archive.clone(), node, config).await.build_segment(1).await.unwrap();
            let direct = SegmentRows::build(&config, 1, &fixture.blocks[8..16]).unwrap();
            assert_eq!(recovered, direct, "recovered senders give identical rows");
        })
        .await;
    }

    /// The run loop publishes every finalized segment and stops on shutdown.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_until_publishes_and_stops() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-run/", &config).await;
            let p = publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let watcher = async {
                while p.next_segment().await.unwrap() < 5 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                tx.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(p.run_until(async { rx.await.unwrap() }), watcher);
            assert_eq!(result, Ok(()), "clean shutdown");
            assert_eq!(p.next_segment().await.unwrap(), 5, "all five finalized segments published");
        })
        .await;
    }

    /// A9: a row above the batch target goes in its own put; a value above 32 MiB is terminal.
    #[test]
    fn oversized_rows_are_isolated() {
        let options = PublisherOptions { max_batch_bytes: 1024, max_batch_rows: 3, ..PublisherOptions::default() };
        let row = |n: u64, len: usize| (ArchiveKey::block(Family::Body, n), Bytes::from(vec![0u8; len]));
        let marker = SegmentMarker { last_hash: B256::ZERO, row_count: 0, digest: B256::ZERO };
        let rows = SegmentRows {
            segment: 0,
            rows: vec![row(0, 10), row(1, 10), row(2, 2048), row(3, 10), row(4, 10), row(5, 10), row(6, 10), row(7, 600), row(8, 600)],
            marker,
        };
        assert_eq!(
            options.plan_chunks(&rows, 13).unwrap(),
            vec![0..2, 2..3, 3..6, 6..8, 8..9],
            "oversized row alone, row and byte limits respected"
        );
        let huge = SegmentRows { rows: vec![row(0, MAX_VALUE_LEN + 1)], ..rows };
        assert!(matches!(options.plan_chunks(&huge, 13), Err(ArchiveError::ValueTooLarge { len, .. }) if len == MAX_VALUE_LEN + 1), "value above 32 MiB");
        assert_eq!(PutSize::entry(10, 0), 1 + 1 + 1 + 1 + 10, "empty value omitted from the encoding");
    }
}
