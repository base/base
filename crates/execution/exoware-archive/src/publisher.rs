//! CAS-free segment publication.
//!
//! Rows are a pure function of finalized block data, so any number of exporters may publish
//! the same segment: they write identical bytes. A segment becomes visible only when its
//! marker is written, after every row is acknowledged and after marker `k − 1` exists.
//! Marker checks before and during publication are diagnostics, not fencing: concurrent
//! *incompatible* writers are unsupported without conditional writes.

use std::{
    future::Future,
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, hex};
use bytes::Bytes;
use exoware_sdk::limits::{MAX_REQUEST_MESSAGE_BYTES, MAX_VALUE_LEN};
use tracing::{debug, error, info, warn};

use crate::{
    ArchiveConfig, ArchiveError, ArchiveKey, ArchiveMetrics, ArchiveStore, BlockSource,
    BlockVerifier, DEFAULT_PRUNE_GRACE_SEGMENTS, ExowareArchive, SegmentBuilder, SegmentLoader,
    SegmentMarker, SegmentRows,
};

/// Default blocks loaded from the source at a time while a segment is published.
pub const DEFAULT_EXPORT_BATCH_BLOCKS: u64 = 256;

/// Default target size of one `Put` (16 `MiB`).
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
    /// Blocks loaded, encoded and written at a time; bounds the exporter's block memory.
    pub export_batch_blocks: u64,
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
            export_batch_blocks: DEFAULT_EXPORT_BATCH_BLOCKS,
        }
    }
}

impl PublisherOptions {
    /// Checks value ranges.
    pub fn validate(&self) -> Result<(), ArchiveError> {
        let invalid =
            |reason: &str| Err(ArchiveError::InvalidConfig { reason: reason.to_string() });
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
        if self.export_batch_blocks == 0 {
            return invalid("export_batch_blocks must be positive");
        }
        Ok(())
    }

    /// Splits a segment's rows into `Put`s; see [`Self::plan_row_chunks`].
    pub fn plan_chunks(
        &self,
        rows: &SegmentRows,
        prefix_len: usize,
    ) -> Result<Vec<Range<usize>>, ArchiveError> {
        self.plan_row_chunks(rows.segment, &rows.rows, prefix_len)
    }

    /// Splits rows of `segment` into `Put`s of at most `max_batch_rows` rows and
    /// `max_batch_bytes` encoded bytes. A row larger than the byte target gets its own `Put`; a
    /// value above the store's 32 `MiB` limit is a terminal error for the segment.
    pub fn plan_row_chunks(
        &self,
        segment: u64,
        rows: &[(Bytes, Bytes)],
        prefix_len: usize,
    ) -> Result<Vec<Range<usize>>, ArchiveError> {
        let mut chunks = Vec::new();
        let mut start = 0;
        let mut bytes = 0;
        for (i, (key, value)) in rows.iter().enumerate() {
            if value.len() > MAX_VALUE_LEN {
                return Err(ArchiveError::ValueTooLarge {
                    segment,
                    key: hex::encode(key),
                    len: value.len(),
                    max: MAX_VALUE_LEN,
                });
            }
            let entry = PutSize::entry(prefix_len + key.len(), value.len());
            let full = i > start
                && (i - start >= self.max_batch_rows || bytes + entry > self.max_batch_bytes);
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
        if start < rows.len() {
            chunks.push(start..rows.len());
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
    /// A marker exists, read while the local marker was still being built.
    Present(SegmentMarker),
}

/// The local marker a marker recheck compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerCheck<'a> {
    /// Block rows are still streaming, so the local marker is not known yet: any stored marker
    /// is [`MarkerState::Present`] and is compared once the segment is built.
    Pending,
    /// The local marker: an equal one is [`MarkerState::Equal`], a different one quarantines.
    Known(&'a SegmentMarker),
}

/// Publishes segments from a [`BlockSource`] into an archive.
///
/// The only public publication paths are [`Self::publish_segment`], [`Self::step`] and
/// [`Self::run_until`], which all check finality, the predecessor marker and the parent link
/// before writing. Raw row writes stay crate-internal:
///
/// ```no_run
/// # use base_exoware_archive::{BlockSource, ExowareStore, SegmentPublisher};
/// async fn checked<B: BlockSource + 'static>(p: &SegmentPublisher<ExowareStore, B>) {
///     let _ = p.publish_segment(0).await;
/// }
/// ```
///
/// ```compile_fail
/// # use base_exoware_archive::{BlockSource, ExowareStore, MarkerCheck, SegmentPublisher};
/// async fn bypass<B: BlockSource + 'static>(p: &SegmentPublisher<ExowareStore, B>) {
///     let _ = p.put_with_recovery(0, MarkerCheck::Pending, &[]).await;
/// }
/// ```
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

    /// Loads blocks `first..=last`, verifies them and pushes them into `builder` on the
    /// blocking pool. Returns the builder, the block-family rows of the batch and the parent
    /// hash of its first block; the blocks themselves are dropped before this returns, so at
    /// most one batch of [`crate::BlockData`] is alive at a time.
    pub async fn build_batch(
        &self,
        mut builder: SegmentBuilder,
        first: u64,
        last: u64,
    ) -> Result<(SegmentBuilder, Vec<(Bytes, Bytes)>, B256), ArchiveError>
    where
        B: 'static,
    {
        let source = Arc::clone(&self.source);
        let verifier = self.verifier.clone();
        tokio::task::spawn_blocking(move || {
            let blocks = SegmentLoader::load_range(source.as_ref(), first, last)?;
            for block in &blocks {
                verifier.verify_block(block).map_err(|e| ArchiveError::InvalidBlock {
                    block: block.number(),
                    reason: e.to_string(),
                })?;
            }
            let parent = blocks.first().map(|b| b.header.parent_hash).unwrap_or_default();
            let rows = builder.push_batch(&blocks)?;
            Ok((builder, rows, parent))
        })
        .await
        .map_err(|e| ArchiveError::Local { reason: format!("segment build task failed: {e}") })?
    }

    /// Batch ranges of `segment`: `export_batch_blocks` consecutive blocks each.
    pub fn batches(&self, segment: u64) -> impl Iterator<Item = (u64, u64)> {
        let (first, last) = (self.config.first_block(segment), self.config.last_block(segment));
        let size = self.options.export_batch_blocks;
        (first..=last).step_by(size as usize).map(move |start| (start, last.min(start + size - 1)))
    }

    /// Builds every row of `segment` in memory, batch by batch. Publication streams instead;
    /// this is for comparisons and tests.
    pub async fn build_segment(&self, segment: u64) -> Result<SegmentRows, ArchiveError>
    where
        B: 'static,
    {
        let mut builder = SegmentBuilder::new(&self.config, segment);
        let mut block_rows = Vec::new();
        for (first, last) in self.batches(segment) {
            let (next, rows, _) = self.build_batch(builder, first, last).await?;
            builder = next;
            block_rows.extend(rows);
        }
        let tail = builder.finish()?;
        let mut rows = tail.index_rows;
        rows.extend(block_rows);
        rows.sort();
        Ok(SegmentRows {
            segment,
            rows,
            first_parent_hash: tail.first_parent_hash,
            marker: tail.marker,
        })
    }

    /// Node's finalized block, read on the blocking pool.
    pub async fn finalized(&self) -> Result<Option<u64>, ArchiveError>
    where
        B: 'static,
    {
        let source = Arc::clone(&self.source);
        tokio::task::spawn_blocking(move || source.finalized_block_number())
            .await
            .map_err(|e| ArchiveError::Local { reason: format!("finalized lookup failed: {e}") })?
    }

    /// Fails with [`ArchiveError::NotFinalized`] unless the node's finalized block is at or
    /// above the segment's last block.
    pub fn require_finalized(
        &self,
        segment: u64,
        finalized: Option<u64>,
    ) -> Result<(), ArchiveError> {
        let last_block = self.config.last_block(segment);
        if finalized.is_none_or(|finalized| finalized < last_block) {
            return Err(ArchiveError::NotFinalized { segment, last_block, finalized });
        }
        Ok(())
    }

    /// Publishes `segment` from the local source once the node has finalized it.
    pub async fn publish_segment(&self, segment: u64) -> Result<PublishOutcome, ArchiveError>
    where
        B: 'static,
    {
        self.require_finalized(segment, self.finalized().await?)?;
        self.publish_finalized(segment).await
    }

    /// Publishes `segment` whose finality the caller has checked, streaming it in batches of
    /// `export_batch_blocks` blocks: each batch is loaded, verified, encoded, folded into the
    /// digest and its block-family rows written chunk by chunk before the next batch is
    /// loaded. Index rows and postings accumulate and are written after the last batch, then
    /// marker `k` last, so a partial segment stays invisible exactly as before.
    ///
    /// Checks, in order: marker `k − 1` exists; an existing marker `k` is compared with a
    /// rebuild that writes nothing (equal is [`PublishOutcome::AlreadyPublished`], different
    /// quarantines); before the first row, the first block's parent must equal marker
    /// `k − 1`'s last hash; ambiguous `Put` failures recheck marker `k`; before the marker put,
    /// marker `k` and the predecessor link are checked again.
    pub(crate) async fn publish_finalized(
        &self,
        segment: u64,
    ) -> Result<PublishOutcome, ArchiveError>
    where
        B: 'static,
    {
        self.require_predecessor(segment, None).await?;
        let session = self.archive.store.session();
        let stored = ExowareArchive::<S>::marker(&session, segment).await?;
        let prefix_len = self.archive.store.key_prefix_len();
        let mut builder = SegmentBuilder::new(&self.config, segment);
        let mut writing = stored.is_none();
        let mut puts = 0;
        for (first, last) in self.batches(segment) {
            let (next, rows, parent) = self.build_batch(builder, first, last).await?;
            builder = next;
            if first == self.config.first_block(segment) && writing {
                self.require_predecessor(segment, Some(parent)).await?;
            }
            if !writing {
                continue;
            }
            for chunk in self.options.plan_row_chunks(segment, &rows, prefix_len)? {
                match self.put_with_recovery(segment, MarkerCheck::Pending, &rows[chunk]).await? {
                    (attempts, MarkerState::Absent) => puts += attempts,
                    (attempts, _) => {
                        // Marker `k` appeared mid-stream: stop writing, finish the rebuild
                        // and compare below.
                        puts += attempts;
                        writing = false;
                        break;
                    }
                }
            }
        }
        let tail = builder.finish()?;
        let marker = tail.marker;
        if !writing {
            return match self.marker_state(segment, &marker).await? {
                MarkerState::Equal => Ok(PublishOutcome::AlreadyPublished),
                state => Err(ArchiveError::unavailable(
                    crate::Family::Marker,
                    format!("marker {segment} disappeared during publication: {state:?}"),
                )),
            };
        }
        for chunk in self.options.plan_row_chunks(segment, &tail.index_rows, prefix_len)? {
            match self
                .put_with_recovery(segment, MarkerCheck::Known(&marker), &tail.index_rows[chunk])
                .await?
            {
                (attempts, MarkerState::Absent) => puts += attempts,
                _ => return Ok(PublishOutcome::AlreadyPublished),
            }
        }
        if self.marker_state(segment, &marker).await? == MarkerState::Equal {
            return Ok(PublishOutcome::AlreadyPublished);
        }
        self.require_predecessor(segment, Some(tail.first_parent_hash)).await?;
        let marker_row = [(ArchiveKey::marker(segment), Bytes::from(marker.encode()))];
        if self.put_with_recovery(segment, MarkerCheck::Known(&marker), &marker_row).await?.1
            == MarkerState::Equal
        {
            return Ok(PublishOutcome::AlreadyPublished);
        }
        ArchiveMetrics::segments_published().increment(1);
        ArchiveMetrics::highest_marker().set(segment as f64);
        info!(segment, rows = marker.row_count, puts, digest = %marker.digest, "published segment");
        Ok(PublishOutcome::Published { puts, rows: marker.row_count })
    }

    /// Fails with [`ArchiveError::Contiguity`] unless marker `segment − 1` exists or `segment`
    /// is the start segment. With `parent_hash`, the marker's `last_hash` must equal it, else
    /// export is quarantined with [`ArchiveError::LinkageQuarantined`]; this also catches a
    /// bogus highest marker.
    pub async fn require_predecessor(
        &self,
        segment: u64,
        parent_hash: Option<B256>,
    ) -> Result<(), ArchiveError> {
        if segment <= self.config.start_segment {
            return Ok(());
        }
        let session = self.archive.store.session();
        let Some(predecessor) = ExowareArchive::<S>::marker(&session, segment - 1).await? else {
            return Err(ArchiveError::Contiguity { segment, missing: segment - 1 });
        };
        if let Some(parent_hash) = parent_hash
            && parent_hash != predecessor.last_hash
        {
            ArchiveMetrics::quarantined().set(1.0);
            error!(
                segment,
                predecessor_hash = %predecessor.last_hash,
                parent_hash = %parent_hash,
                "segment does not extend the stored predecessor marker; export quarantined"
            );
            return Err(ArchiveError::LinkageQuarantined {
                segment,
                predecessor_hash: predecessor.last_hash,
                parent_hash,
            });
        }
        Ok(())
    }

    /// Compares marker `segment` with the local one. A different marker quarantines export.
    pub async fn marker_state(
        &self,
        segment: u64,
        local: &SegmentMarker,
    ) -> Result<MarkerState, ArchiveError> {
        self.check_marker(segment, MarkerCheck::Known(local)).await
    }

    /// Reads marker `segment` and compares it as `check` says.
    pub async fn check_marker(
        &self,
        segment: u64,
        check: MarkerCheck<'_>,
    ) -> Result<MarkerState, ArchiveError> {
        let session = self.archive.store.session();
        let local = match (ExowareArchive::<S>::marker(&session, segment).await?, check) {
            (None, _) => return Ok(MarkerState::Absent),
            (Some(stored), MarkerCheck::Pending) => return Ok(MarkerState::Present(stored)),
            (Some(stored), MarkerCheck::Known(local)) => (stored, local),
        };
        match local {
            (stored, local) if stored == *local => Ok(MarkerState::Equal),
            (stored, local) => {
                ArchiveMetrics::quarantined().set(1.0);
                error!(
                    segment,
                    stored_digest = %stored.digest,
                    local_digest = %local.digest,
                    "stored segment marker differs from local rows; export quarantined"
                );
                Err(ArchiveError::Quarantined {
                    segment,
                    stored: stored.digest,
                    local: local.digest,
                })
            }
        }
    }

    /// Writes one chunk. After an ambiguous failure it backs off and rereads marker `segment`
    /// (retrying the read a bounded number of times): an equal marker means the segment is
    /// complete; a confirmed absent one replays the same rows. A recheck that keeps failing
    /// aborts without another `Put`. The marker is rechecked after the last failed attempt too.
    /// Returns the number of `Put`s issued and the marker state that ended the loop.
    pub(crate) async fn put_with_recovery(
        &self,
        segment: u64,
        marker: MarkerCheck<'_>,
        chunk: &[(Bytes, Bytes)],
    ) -> Result<(usize, MarkerState), ArchiveError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let started = Instant::now();
            let err = match self.archive.store.put(chunk).await {
                Ok(sequence) => {
                    ArchiveMetrics::put_latency().record(started.elapsed().as_secs_f64());
                    debug!(segment, rows = chunk.len(), sequence, "put acknowledged");
                    return Ok((attempt, MarkerState::Absent));
                }
                Err(err) => err,
            };
            warn!(segment, attempt, error = %err, "put outcome unknown; rechecking marker");
            tokio::time::sleep(self.options.backoff(attempt)).await;
            match self.recheck_marker(segment, marker).await? {
                MarkerState::Absent if attempt >= self.options.max_put_attempts => {
                    warn!(segment, attempt, error = %err, "put failed; giving up for this round");
                    return Err(err);
                }
                MarkerState::Absent => ArchiveMetrics::put_retries().increment(1),
                state => return Ok((attempt, state)),
            }
        }
    }

    /// Reads marker `segment` for [`Self::put_with_recovery`], retrying failed reads up to
    /// `max_put_attempts` times. Quarantine and the last read error are returned.
    pub(crate) async fn recheck_marker(
        &self,
        segment: u64,
        marker: MarkerCheck<'_>,
    ) -> Result<MarkerState, ArchiveError> {
        let mut read = 0;
        loop {
            read += 1;
            match self.check_marker(segment, marker).await {
                Ok(state) => return Ok(state),
                Err(err @ ArchiveError::Quarantined { .. }) => return Err(err),
                Err(err) if read >= self.options.max_put_attempts => {
                    warn!(segment, error = %err, "marker recheck failed; aborting without replay");
                    return Err(err);
                }
                Err(err) => {
                    warn!(segment, read, error = %err, "marker recheck failed; retrying the read");
                    tokio::time::sleep(self.options.backoff(read)).await;
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
        let finalized = self.finalized().await?;
        if let Some(finalized) = finalized {
            ArchiveMetrics::lag_blocks().set(self.lag_blocks(segment, finalized) as f64);
        }
        if self.require_finalized(segment, finalized).is_err() {
            return Ok(StepOutcome::Waiting { segment, finalized });
        }
        let outcome = self.publish_finalized(segment).await?;
        self.archive.prune(&self.config, Some(segment), self.options.prune_grace_segments).await?;
        Ok(StepOutcome::Advanced { segment, outcome })
    }

    /// Finalized blocks not yet published when `next` is the next segment to publish:
    /// `finalized + 1 − first_block(next)`, saturating. Before marker 0 exists this counts
    /// from the start segment, so a fresh exporter that cannot publish is visible.
    pub const fn lag_blocks(&self, next: u64, finalized: u64) -> u64 {
        finalized.saturating_add(1).saturating_sub(self.config.first_block(next))
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
    ///
    /// Shutdown is checked first and raced against every round, so a signaled exporter starts
    /// no further round and abandons one in flight at its next await point (between chunks,
    /// retries or backoffs). Abandoning a round is the same as a crash: the marker is written
    /// last and replays write identical bytes, so a later round completes the segment.
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
                let reconciled = tokio::select! {
                    biased;
                    () = &mut shutdown => return Ok(()),
                    reconciled = self.reconcile_prune() => reconciled,
                };
                if let Err(err) = reconciled {
                    warn!(error = %err, "reconcile prune failed");
                }
            }
            let stepped = tokio::select! {
                biased;
                () = &mut shutdown => return Ok(()),
                stepped = self.step() => stepped,
            };
            let delay = match stepped {
                Ok(StepOutcome::Advanced { .. }) => {
                    failures = 0;
                    Duration::ZERO
                }
                Ok(StepOutcome::Waiting { .. }) => {
                    failures = 0;
                    self.options.idle_interval
                }
                Err(
                    err @ (ArchiveError::Quarantined { .. }
                    | ArchiveError::LinkageQuarantined { .. }
                    | ArchiveError::ConfigMismatch { .. }
                    | ArchiveError::ConfigMissing
                    | ArchiveError::ValueTooLarge { .. }),
                ) => {
                    error!(error = %err, "archive export halted");
                    return Err(err);
                }
                Err(err @ ArchiveError::LocalDataPruned { .. }) => {
                    failures += 1;
                    error!(
                        error = %err,
                        failures,
                        "exporter lags the node's retention; the archive has a gap until another node publishes it"
                    );
                    self.options.backoff(failures)
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
        ArchiveReader, ArchiveStore, AuditFinding, BlockCodec, ExowareStore, FORMAT_VERSION,
        Family, LocalAvailability, ProvisionOutcome, ReaderLimits, SegmentAuditor, StoreSession,
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
    async fn clean_dump(
        sim: &TestSimulator,
        namespace: &str,
        fixture: &Arc<ChainFixture>,
        last: u64,
    ) -> Vec<(Bytes, Bytes)> {
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
                            // One batch of block rows, then the index rows.
                            puts: 2,
                            rows: SegmentRows::build(
                                &config,
                                segment,
                                &fixture.blocks[(segment * TEST_N) as usize
                                    ..((segment + 1) * TEST_N) as usize]
                            )
                            .unwrap()
                            .marker
                            .row_count,
                        },
                    },
                    "segment {segment}: one block put and one index put"
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
                assert_eq!(
                    reader.block_number_by_hash(block.hash()).await.unwrap(),
                    Some(n),
                    "block locator {n}"
                );
                for (index, tx) in block.body.transactions.iter().enumerate() {
                    let found =
                        reader.transaction_by_hash(tx.tx_hash()).await.unwrap().expect("tx found");
                    assert_eq!(
                        (found.block_number, found.index, &found.transaction),
                        (n, index as u32, tx),
                        "tx locator {n}/{index}"
                    );
                    assert_eq!(found.block_hash, block.hash(), "tx block hash {n}/{index}");
                }
                let accounts = BlockCodec::decode_account_changes(
                    &session
                        .get(&ArchiveKey::block(Family::AccountChanges, n))
                        .await
                        .unwrap()
                        .expect("row"),
                )
                .unwrap();
                let mut expected = block.account_changes.clone();
                expected.sort_by_key(|c| c.address);
                assert_eq!(accounts, expected, "account changes {n}");
                let storage = BlockCodec::decode_storage_changes(
                    &session
                        .get(&ArchiveKey::block(Family::StorageChanges, n))
                        .await
                        .unwrap()
                        .expect("row"),
                )
                .unwrap();
                let mut expected = block.storage_changes.clone();
                expected.sort_by_key(|c| (c.address, c.slot));
                assert_eq!(storage, expected, "storage changes {n}");
            }
            assert_eq!(
                reader.block(32).await.unwrap_err(),
                ArchiveError::NotArchived { block: 32, highest: Some(31) },
                "block above H is not archived"
            );
            assert_eq!(
                reader.block_number_by_hash(B256::repeat_byte(9)).await.unwrap(),
                None,
                "unknown hash"
            );
            assert_eq!(
                reader
                    .transaction_by_hash(fixture.blocks[33].body.transactions[1].tx_hash())
                    .await
                    .unwrap(),
                None,
                "unarchived tx"
            );
        })
        .await;
    }

    /// Test 4 / A1: two publishers over independently built sources race on the same
    /// segments, held at a barrier until both are about to issue their first `Put`; the store
    /// ends with exactly one marker per segment and the same logical contents as a
    /// single-publisher store.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_publishers_converge() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture_a = Arc::new(ChainFixture::standard());
            let fixture_b = Arc::new(ChainFixture::standard());
            let config = fixture_a.config(TEST_N, 0, None);
            let shared = sim.archive("t4-shared/", &config).await;
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let gated = |fixture: &Arc<ChainFixture>| {
                let store = FaultyStore::new(Arc::clone(&shared.store), vec![])
                    .with_first_put_barrier(Arc::clone(&barrier));
                let archive = ExowareArchive::new(Arc::new(store));
                publisher(archive, FixtureNode::new(Arc::clone(fixture)), config)
            };
            let (a, b) = tokio::join!(gated(&fixture_a), gated(&fixture_b));
            let run = |p: SegmentPublisher<FaultyStore<ExowareStore>, FixtureNode>| async move {
                let mut outcomes = Vec::new();
                for _ in 0..5 {
                    match p.step().await.expect("step succeeds") {
                        StepOutcome::Advanced { segment, outcome } => {
                            outcomes.push((segment, outcome))
                        }
                        StepOutcome::Waiting { .. } => break,
                    }
                }
                (outcomes, p.archive.store.put_count())
            };
            let ((from_a, puts_a), (from_b, puts_b)) = tokio::join!(run(a), run(b));
            // A writer that falls behind resumes at the shared highest marker + 1, so it may
            // skip segments the other one already finished; together they cover all five.
            let mut covered: Vec<u64> =
                from_a.iter().chain(&from_b).map(|(segment, _)| *segment).collect();
            covered.sort_unstable();
            covered.dedup();
            assert_eq!(
                covered,
                vec![0, 1, 2, 3, 4],
                "the writers advanced through every segment: {from_a:?} {from_b:?}"
            );
            for outcomes in [&from_a, &from_b] {
                assert!(
                    outcomes.windows(2).all(|w| w[0].0 < w[1].0),
                    "each writer advances strictly upward: {outcomes:?}"
                );
            }
            assert!(
                puts_a >= 1 && puts_b >= 1,
                "both writers issued puts after the barrier: {puts_a} {puts_b}"
            );
            assert_eq!(
                (from_a[0].0, from_b[0].0),
                (0, 0),
                "both writers were inside segment 0's publication when the barrier released them"
            );

            let rows = dump(shared.store.as_ref()).await;
            let markers: Vec<_> = rows
                .iter()
                .filter(|(k, _)| ArchiveKey::family(k) == Some(Family::Marker))
                .collect();
            assert_eq!(markers.len(), 5, "exactly one marker per segment");
            for (segment, (key, value)) in markers.iter().enumerate() {
                let built = SegmentRows::build(
                    &config,
                    segment as u64,
                    &fixture_a.blocks[segment * 8..segment * 8 + 8],
                )
                .unwrap();
                assert_eq!(key, &&ArchiveKey::marker(segment as u64), "marker key {segment}");
                assert_eq!(
                    SegmentMarker::decode(value).unwrap(),
                    built.marker,
                    "marker {segment} digest"
                );
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
            let p = SegmentPublisher::open(
                archive.clone(),
                Arc::new(node.clone()),
                verifier,
                config,
                options,
            )
            .await
            .unwrap();
            let err = p.publish_segment(0).await.unwrap_err();
            assert_eq!(
                err,
                ArchiveError::store("put", "injected failure"),
                "fifth failed attempt surfaces"
            );
            assert_eq!(faulty.put_count(), 6, "one acked chunk plus five failed attempts");
            assert_eq!(p.next_segment().await.unwrap(), 0, "H unchanged");
            let r = reader(plain.clone(), node).await;
            assert_eq!(
                r.block(0).await.unwrap_err(),
                ArchiveError::NotArchived { block: 0, highest: None },
                "segment invisible"
            );
            let partial = dump(plain.store.as_ref()).await;
            assert!(partial.len() > 1, "first chunk was written");
            assert!(
                partial.iter().all(|(k, _)| ArchiveKey::family(k) != Some(Family::Marker)),
                "no marker"
            );

            let outcome = p.publish_segment(0).await.unwrap();
            assert!(
                matches!(outcome, PublishOutcome::Published { .. }),
                "rerun publishes, got {outcome:?}"
            );
            for segment in 1..5 {
                p.publish_segment(segment).await.unwrap();
            }
            assert!(
                dump(plain.store.as_ref()).await
                    == clean_dump(&sim, "t5-clean/", &fixture, 4).await,
                "identical bytes"
            );
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
            let p =
                publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            assert_eq!(
                p.publish_segment(2).await.unwrap_err(),
                ArchiveError::Contiguity { segment: 2, missing: 1 },
                "early check"
            );
            assert_eq!(dump(archive.store.as_ref()).await.len(), 1, "only the config row exists");
            p.publish_segment(0).await.unwrap();
            assert_eq!(
                p.publish_finalized(2).await.unwrap_err(),
                ArchiveError::Contiguity { segment: 2, missing: 1 },
                "internal path checks too"
            );
            assert_eq!(
                dump(archive.store.as_ref())
                    .await
                    .iter()
                    .filter(|(k, _)| ArchiveKey::number(k).is_some_and(|n| n >= 16)
                        && ArchiveKey::family(k).is_some_and(|f| f.keyed_by_block()))
                    .count(),
                0,
                "no block row of segment 2 was written"
            );
            let session = archive.store.session();
            assert_eq!(
                ExowareArchive::<ExowareStore>::marker(&session, 2).await.unwrap(),
                None,
                "marker 2 not written"
            );
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
                SegmentPublisher::open(
                    archive,
                    Arc::clone(&node),
                    fixture.verifier(),
                    config,
                    fast_options(),
                )
            };
            let empty = ExowareArchive::new(sim.store("t7-empty/"));
            assert_eq!(
                open(empty, config).await.unwrap_err(),
                ArchiveError::ConfigMissing,
                "no implicit config"
            );
            let archive = sim.archive("t7/", &config).await;
            assert_eq!(
                archive.provision(&config).await.unwrap(),
                ProvisionOutcome::AlreadyProvisioned,
                "idempotent provision"
            );
            let unsupported = ArchiveConfig { format: FORMAT_VERSION + 1, ..config };
            assert!(
                matches!(
                    open(archive.clone(), unsupported).await.unwrap_err(),
                    ArchiveError::InvalidConfig { .. }
                ),
                "exporter refuses an unsupported format"
            );
            assert!(
                matches!(
                    archive.provision(&unsupported).await.unwrap_err(),
                    ArchiveError::InvalidConfig { .. }
                ),
                "provision refuses an unsupported format"
            );
            let fresh = ExowareArchive::new(sim.store("t7-fresh/"));
            assert!(
                matches!(
                    fresh.provision(&unsupported).await.unwrap_err(),
                    ArchiveError::InvalidConfig { .. }
                ),
                "fresh provision refuses an unsupported format"
            );
            assert_eq!(
                dump(fresh.store.as_ref()).await.len(),
                0,
                "nothing written by a refused provision"
            );
            let stored_unsupported = ExowareArchive::new(sim.store("t7-unsupported/"));
            stored_unsupported
                .store
                .put(&[(ArchiveKey::config(), Bytes::from(unsupported.encode()))])
                .await
                .unwrap();
            assert!(
                matches!(
                    stored_unsupported.provision(&config).await.unwrap_err(),
                    ArchiveError::ConfigMismatch { .. }
                ),
                "provision against a stored unsupported row"
            );
            assert!(
                matches!(
                    ArchiveReader::open(
                        stored_unsupported.clone(),
                        fixture.verifier(),
                        Arc::clone(&node),
                        config.chain_id,
                        config.genesis_hash,
                        ReaderLimits::default()
                    )
                    .await
                    .unwrap_err(),
                    ArchiveError::Decode { family: Family::Config, .. }
                ),
                "reader refuses a stored unsupported row"
            );
            let variants = [
                ArchiveConfig { blocks_per_segment: 16, ..config },
                ArchiveConfig { chain_id: 1, ..config },
                ArchiveConfig { genesis_hash: B256::repeat_byte(1), ..config },
            ];
            for variant in variants {
                assert!(
                    matches!(
                        open(archive.clone(), variant).await.unwrap_err(),
                        ArchiveError::ConfigMismatch { .. }
                    ),
                    "exporter refuses {variant}"
                );
                assert!(
                    matches!(
                        archive.provision(&variant).await.unwrap_err(),
                        ArchiveError::ConfigMismatch { .. }
                    ),
                    "provision refuses {variant}"
                );
            }
            let open_reader = |chain_id: u64, genesis: B256| {
                ArchiveReader::open(
                    archive.clone(),
                    fixture.verifier(),
                    Arc::clone(&node),
                    chain_id,
                    genesis,
                    ReaderLimits::default(),
                )
            };
            assert!(
                matches!(
                    open_reader(1, config.genesis_hash).await.unwrap_err(),
                    ArchiveError::LinkDisabled { .. }
                ),
                "chain id mismatch"
            );
            assert!(
                matches!(
                    open_reader(config.chain_id, B256::ZERO).await.unwrap_err(),
                    ArchiveError::LinkDisabled { .. }
                ),
                "genesis mismatch"
            );
            assert!(
                open_reader(config.chain_id, config.genesis_hash).await.is_ok(),
                "matching identity opens"
            );
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
            let bogus = SegmentMarker {
                last_hash: B256::ZERO,
                row_count: 1,
                digest: B256::repeat_byte(0xbb),
            };
            archive
                .store
                .put(&[(ArchiveKey::marker(0), Bytes::from(bogus.encode()))])
                .await
                .unwrap();
            let p =
                publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            let local = p.build_segment(0).await.unwrap().marker;
            assert_eq!(
                p.publish_segment(0).await.unwrap_err(),
                ArchiveError::Quarantined { segment: 0, stored: bogus.digest, local: local.digest },
                "export quarantined"
            );
            assert_eq!(
                dump(archive.store.as_ref()).await.len(),
                2,
                "only config and the bogus marker"
            );
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
            let recovered =
                publisher(archive.clone(), node, config).await.build_segment(1).await.unwrap();
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
            let p =
                publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
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

    /// P1: a bogus highest marker (wrong `last_hash`) makes the run loop quarantine before
    /// writing any row of the next segment, instead of publishing past it.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_until_quarantines_on_bogus_highest_marker() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-bogus/", &config).await;
            publish_through(&archive, &fixture, config, 0).await;
            let bogus = SegmentMarker {
                last_hash: B256::repeat_byte(0xbb),
                row_count: 1,
                digest: B256::repeat_byte(0xcc),
            };
            archive
                .store
                .put(&[(ArchiveKey::marker(1), Bytes::from(bogus.encode()))])
                .await
                .unwrap();
            let before = dump(archive.store.as_ref()).await;
            let p =
                publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            let result = p.run_until(std::future::pending()).await;
            assert_eq!(
                result,
                Err(ArchiveError::LinkageQuarantined {
                    segment: 2,
                    predecessor_hash: bogus.last_hash,
                    parent_hash: fixture.blocks[15].hash(),
                }),
                "run loop halts at segment 2"
            );
            assert!(
                dump(archive.store.as_ref()).await == before,
                "no row of segment 2 was written"
            );
            assert_eq!(p.next_segment().await.unwrap(), 2, "H did not move past the bogus marker");
        })
        .await;
    }

    /// B4: the public publication path refuses a segment above the node's finalized block.
    #[tokio::test(flavor = "multi_thread")]
    async fn publish_segment_requires_finality() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-final/", &config).await;
            let mut node = FixtureNode::new(Arc::clone(&fixture));
            node.finalized = 6;
            let p = publisher(archive.clone(), node, config).await;
            assert_eq!(
                p.publish_segment(0).await.unwrap_err(),
                ArchiveError::NotFinalized { segment: 0, last_block: 7, finalized: Some(6) },
                "segment 0 ends above finalized block 6"
            );
            assert_eq!(dump(archive.store.as_ref()).await.len(), 1, "only the config row exists");
        })
        .await;
    }

    /// B5: when the marker recheck after a failed put keeps failing, the publisher aborts
    /// without replaying: exactly zero additional puts.
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_recheck_does_not_replay() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let plain = sim.archive("t-recheck/", &config).await;
            let faulty = Arc::new(FaultyStore::new(
                Arc::clone(&plain.store),
                vec![PutFault { when: PutMatch::AtOrAfter(0), kind: FaultKind::Fail, times: 1 }],
            ));
            let p = SegmentPublisher::open(
                ExowareArchive::new(Arc::clone(&faulty)),
                Arc::new(FixtureNode::new(Arc::clone(&fixture))),
                fixture.verifier(),
                config,
                fast_options(),
            )
            .await
            .unwrap();
            let rows = p.build_segment(0).await.unwrap();
            faulty.fail_reads(p.options.max_put_attempts);
            let err = p
                .put_with_recovery(0, MarkerCheck::Known(&rows.marker), &rows.rows[..1])
                .await
                .unwrap_err();
            assert_eq!(
                err,
                ArchiveError::store("get", "injected read failure"),
                "the recheck error surfaces"
            );
            assert_eq!(faulty.put_count(), 1, "exactly the one failed put, zero replays");
            assert_eq!(
                faulty.failing_reads.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "every read retry was used"
            );
            let (puts, state) = p
                .put_with_recovery(0, MarkerCheck::Known(&rows.marker), &rows.rows[..1])
                .await
                .unwrap();
            assert_eq!(
                (puts, state),
                (1, MarkerState::Absent),
                "a healthy store acknowledges the next put"
            );
        })
        .await;
    }

    /// R5: a shutdown signaled before the loop starts issues no put at all, and one signaled
    /// during a round's retry backoff ends the round at that await point instead of after it.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_is_checked_before_and_inside_rounds() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let plain = sim.archive("t-shutdown/", &config).await;
            let faulty = Arc::new(FaultyStore::new(
                Arc::clone(&plain.store),
                vec![PutFault { when: PutMatch::AtOrAfter(0), kind: FaultKind::Fail, times: 100 }],
            ));
            let open = |options| {
                SegmentPublisher::open(
                    ExowareArchive::new(Arc::clone(&faulty)),
                    Arc::new(FixtureNode::new(Arc::clone(&fixture))),
                    fixture.verifier(),
                    config,
                    options,
                )
            };
            let p = open(fast_options()).await.unwrap();
            p.run_until(std::future::ready(()))
                .await
                .expect("an already signaled shutdown is a clean exit");
            assert_eq!(faulty.put_count(), 0, "no round starts after shutdown was signaled");

            let slow = PublisherOptions {
                initial_backoff: Duration::from_secs(30),
                max_backoff: Duration::from_secs(30),
                ..fast_options()
            };
            let p = open(slow).await.unwrap();
            let started = Instant::now();
            p.run_until(tokio::time::sleep(Duration::from_millis(500))).await.expect("clean exit");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "shutdown ended the 30 s backoff, took {:?}",
                started.elapsed()
            );
            assert_eq!(faulty.put_count(), 1, "exactly the one failed put before the backoff");
            assert!(
                ExowareArchive::<FaultyStore<ExowareStore>>::marker(&faulty.session(), 0)
                    .await
                    .unwrap()
                    .is_none(),
                "the abandoned segment has no marker"
            );
        })
        .await;
    }

    /// A [`FixtureNode`] that counts the blocks loaded per batch. A batch is loaded between
    /// two availability checks (before and after), so the blocks read since the opening check
    /// are the blocks alive at once.
    #[derive(Debug)]
    struct CountingNode {
        inner: FixtureNode,
        /// (availability calls, headers read in the current batch, peak per batch).
        counts: std::sync::Mutex<(u64, u64, u64)>,
    }

    impl CountingNode {
        fn peak(&self) -> u64 {
            self.counts.lock().unwrap().2
        }
    }

    impl BlockSource for CountingNode {
        fn finalized_block_number(&self) -> Result<Option<u64>, ArchiveError> {
            self.inner.finalized_block_number()
        }
        fn availability(&self) -> Result<LocalAvailability, ArchiveError> {
            let mut counts = self.counts.lock().unwrap();
            if counts.0.is_multiple_of(2) {
                counts.1 = 0;
            }
            counts.0 += 1;
            self.inner.availability()
        }
        fn header(&self, number: u64) -> Result<Option<alloy_consensus::Header>, ArchiveError> {
            let mut counts = self.counts.lock().unwrap();
            counts.1 += 1;
            counts.2 = counts.2.max(counts.1);
            self.inner.header(number)
        }
        fn body(&self, number: u64) -> Result<Option<crate::BlockBody>, ArchiveError> {
            self.inner.body(number)
        }
        fn receipts(
            &self,
            number: u64,
        ) -> Result<Option<Vec<base_common_consensus::BaseReceipt>>, ArchiveError> {
            self.inner.receipts(number)
        }
        fn senders(
            &self,
            number: u64,
        ) -> Result<Option<Vec<alloy_primitives::Address>>, ArchiveError> {
            self.inner.senders(number)
        }
        fn account_changes(&self, number: u64) -> Result<Vec<crate::AccountChange>, ArchiveError> {
            self.inner.account_changes(number)
        }
        fn storage_changes(&self, number: u64) -> Result<Vec<crate::StorageChange>, ArchiveError> {
            self.inner.storage_changes(number)
        }
    }

    /// P3: publication streams a segment in `export_batch_blocks` batches. The most blocks
    /// loaded at once equals the batch size, and batch sizes 1, 3 and the whole segment give
    /// identical markers and byte-identical store contents.
    #[tokio::test(flavor = "multi_thread")]
    async fn streamed_publication_is_bounded_and_batch_independent() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let mut dumps = Vec::new();
            for (batch, namespace) in [(1, "t-stream-1/"), (3, "t-stream-3/"), (TEST_N, "t-stream-whole/")] {
                let archive = sim.archive(namespace, &config).await;
                let node = Arc::new(CountingNode {
                    inner: FixtureNode::new(Arc::clone(&fixture)),
                    counts: std::sync::Mutex::new((0, 0, 0)),
                });
                let options = PublisherOptions { export_batch_blocks: batch, ..fast_options() };
                let p = SegmentPublisher::open(archive.clone(), Arc::clone(&node), fixture.verifier(), config, options)
                    .await
                    .unwrap();
                for segment in 0..5 {
                    let outcome = p.publish_segment(segment).await.unwrap();
                    let expected = SegmentRows::build(&config, segment, &fixture.blocks[(segment * TEST_N) as usize..((segment + 1) * TEST_N) as usize]).unwrap();
                    assert!(
                        matches!(outcome, PublishOutcome::Published { rows, .. } if rows == expected.marker.row_count),
                        "batch {batch}: segment {segment} published with every row, got {outcome:?}"
                    );
                    let stored = ExowareArchive::<ExowareStore>::marker(&archive.store.session(), segment).await.unwrap();
                    assert_eq!(stored, Some(expected.marker), "batch {batch}: marker {segment} equals the one-batch build");
                }
                assert_eq!(node.peak(), batch, "batch {batch}: at most {batch} blocks loaded at once");
                assert!(
                    SegmentAuditor::new(archive.clone(), config, fixture.verifier()).audit_segment(1).await.unwrap().is_clean(),
                    "batch {batch}: the streamed segment audits clean"
                );
                dumps.push(dump(archive.store.as_ref()).await);
            }
            assert!(dumps[0] == dumps[1], "batch sizes 1 and 3 give byte-identical stores");
            assert!(dumps[1] == dumps[2], "batch size 3 and the whole segment give byte-identical stores");
            assert!(dumps[0] == clean_dump(&sim, "t-stream-clean/", &fixture, 4).await, "equal to the default publisher's store");
        })
        .await;
    }

    /// The audit gives the same result for every batch size.
    #[tokio::test(flavor = "multi_thread")]
    async fn audit_batch_size_does_not_change_findings() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-audit-batches/", &config).await;
            publish_through(&archive, &fixture, config, 1).await;
            let foreign = BlockCodec::encode_receipts(&fixture.blocks[11].receipts);
            archive
                .store
                .put(&[(ArchiveKey::block(Family::Receipts, 10), Bytes::from(foreign))])
                .await
                .unwrap();
            let mut reports = Vec::new();
            for batch in [1, 3, TEST_N] {
                let auditor = SegmentAuditor {
                    batch_blocks: batch,
                    ..SegmentAuditor::new(archive.clone(), config, fixture.verifier())
                };
                assert!(
                    auditor.audit_segment(0).await.unwrap().is_clean(),
                    "batch {batch}: segment 0 clean"
                );
                reports.push(auditor.audit_segment(1).await.unwrap());
            }
            assert!(
                matches!(
                    reports[0].findings.as_slice(),
                    [AuditFinding::BlockInvalid { block: 10, .. }]
                ),
                "the overwritten receipts row is found: {:?}",
                reports[0].findings
            );
            assert_eq!(reports[0], reports[1], "batch sizes 1 and 3 agree");
            assert_eq!(reports[1], reports[2], "batch size 3 and the whole segment agree");
        })
        .await;
    }

    /// R5: lag counts every finalized block not yet published, from the start segment before
    /// marker 0 exists.
    #[test]
    fn lag_counts_from_the_start_segment() {
        let fixture = Arc::new(ChainFixture::standard());
        let config = fixture.config(TEST_N, 2, None);
        let p = SegmentPublisher {
            archive: ExowareArchive::new(Arc::new(FaultyStore::new(
                Arc::new(
                    ExowareStore::new(
                        exoware_sdk::StoreClient::new("http://127.0.0.1:1"),
                        "t-lag/",
                        Duration::from_secs(1),
                    )
                    .unwrap(),
                ),
                vec![],
            ))),
            source: Arc::new(FixtureNode::new(Arc::clone(&fixture))),
            verifier: fixture.verifier(),
            config,
            options: fast_options(),
        };
        assert_eq!(p.lag_blocks(2, 15), 0, "finalized below the start block 16");
        assert_eq!(p.lag_blocks(2, 16), 1, "start block finalized, nothing published");
        assert_eq!(p.lag_blocks(2, 40), 25, "blocks 16..=40 pending before marker 2 exists");
        assert_eq!(p.lag_blocks(4, 40), 9, "after markers 2 and 3, blocks 32..=40 pending");
    }

    /// A9: a row above the batch target goes in its own put; a value above 32 `MiB` is terminal.
    #[test]
    fn oversized_rows_are_isolated() {
        let options = PublisherOptions {
            max_batch_bytes: 1024,
            max_batch_rows: 3,
            ..PublisherOptions::default()
        };
        let row =
            |n: u64, len: usize| (ArchiveKey::block(Family::Body, n), Bytes::from(vec![0u8; len]));
        let marker = SegmentMarker { last_hash: B256::ZERO, row_count: 0, digest: B256::ZERO };
        let rows = SegmentRows {
            segment: 0,
            rows: vec![
                row(0, 10),
                row(1, 10),
                row(2, 2048),
                row(3, 10),
                row(4, 10),
                row(5, 10),
                row(6, 10),
                row(7, 600),
                row(8, 600),
            ],
            first_parent_hash: B256::ZERO,
            marker,
        };
        assert_eq!(
            options.plan_chunks(&rows, 13).unwrap(),
            vec![0..2, 2..3, 3..6, 6..8, 8..9],
            "oversized row alone, row and byte limits respected"
        );
        let huge = SegmentRows { rows: vec![row(0, MAX_VALUE_LEN + 1)], ..rows };
        assert!(
            matches!(options.plan_chunks(&huge, 13), Err(ArchiveError::ValueTooLarge { len, .. }) if len == MAX_VALUE_LEN + 1),
            "value above 32 MiB"
        );
        assert_eq!(
            PutSize::entry(10, 0),
            1 + 1 + 1 + 1 + 10,
            "empty value omitted from the encoding"
        );
    }
}
