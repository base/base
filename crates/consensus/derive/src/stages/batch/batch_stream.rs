//! This module contains the `BatchStream` stage.

use alloc::{boxed::Box, collections::VecDeque, string::ToString, sync::Arc, vec::Vec};
use core::fmt::Debug;

use alloy_eips::BlockNumHash;
use async_trait::async_trait;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_protocol::{
    Batch, BatchDropReason, BatchValidity, BlockInfo, L2BlockInfo, SingleBatch, SpanBatch,
    SpanBatchError,
};

use crate::{
    L2ChainProvider, Metrics, NextBatchProvider, OriginAdvancer, OriginProvider, PipelineError,
    PipelineResult, StageReset,
};

/// Provides [`Batch`]es for the [`BatchStream`] stage.
#[async_trait]
pub trait BatchStreamProvider {
    /// Returns the next [`Batch`] in the [`BatchStream`] stage.
    async fn next_batch(&mut self) -> PipelineResult<Batch>;

    /// Drains the recent `Channel` if an invalid span batch is found post-holocene.
    fn flush(&mut self);
}

/// [`BatchStream`] stage in the derivation pipeline.
///
/// This stage is introduced in the [`Holocene`] upgrade.
/// It slots in between the [`ChannelReader`] and [`BatchQueue`]
/// stages, buffering span batches until they are validated.
///
/// [`Holocene`]: https://specs.base.org/upgrades/holocene/overview
/// [`ChannelReader`]: crate::stages::ChannelReader
/// [`BatchQueue`]: crate::stages::BatchQueue
#[derive(Debug)]
pub struct BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Debug,
    BF: L2ChainProvider + Debug,
{
    /// The previous stage in the derivation pipeline.
    pub prev: P,
    /// There can only be a single staged span batch.
    pub span: Option<SpanBatch>,
    /// Span awaiting contextual resolution and its L1 inclusion block.
    pub pending_span: Option<(SpanBatch, BlockInfo)>,
    /// A buffer of single batches derived from the [`SpanBatch`].
    pub buffer: VecDeque<SingleBatch>,
    /// A reference to the rollup config, used to check
    /// if the [`BatchStream`] stage should be activated.
    pub config: Arc<RollupConfig>,
    /// Used to validate the batches.
    pub fetcher: BF,
}

impl<P, BF> BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Debug,
    BF: L2ChainProvider + Debug,
{
    /// Create a new [`BatchStream`] stage.
    pub const fn new(prev: P, config: Arc<RollupConfig>, fetcher: BF) -> Self {
        Self { prev, span: None, pending_span: None, buffer: VecDeque::new(), config, fetcher }
    }

    /// Returns if the [`BatchStream`] stage is active based on the
    /// origin timestamp and holocene activation timestamp.
    pub fn is_active(&self) -> PipelineResult<bool> {
        let origin = self.prev.origin().ok_or(PipelineError::MissingOrigin.crit())?;
        Ok(self.config.is_holocene_active(origin.timestamp))
    }

    /// Gets a [`SingleBatch`] from the in-memory buffer.
    pub fn get_single_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> Result<Option<SingleBatch>, SpanBatchError> {
        trace!(target: "batch_span", buffer_len = self.buffer.len(), "Attempting to get a SingleBatch from buffer");

        self.try_hydrate_buffer(parent, l1_origins)?;
        Ok(self.buffer.pop_front())
    }

    /// Hydrates the buffer with single batches derived from the span batch, if there is one
    /// queued up.
    pub fn try_hydrate_buffer(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> Result<(), SpanBatchError> {
        if let Some(span) = self.span.take() {
            self.buffer.extend(span.get_singular_batches(l1_origins, parent)?);
        }
        let batch_count = self.buffer.len() as f64;
        Metrics::pipeline_batch_buffer().set(batch_count);
        let batch_size = core::mem::size_of_val(&self.buffer) as f64;
        Metrics::pipeline_batch_mem().set(batch_size);
        Ok(())
    }

    /// Resolves a span's exact first L2 block from the range sharing its header timestamp, using
    /// span coverage and the parent check.
    pub async fn resolve_span_start_block(
        &mut self,
        span: &SpanBatch,
        l2_safe_head: L2BlockInfo,
    ) -> Result<u64, BatchValidity> {
        let Some(first_batch) = span.batches.first() else {
            return Err(BatchValidity::Drop(BatchDropReason::SpanBatchMisalignedTimestamp));
        };

        let next_block_number = l2_safe_head.block_info.number + 1;
        let Some(timestamp_range) =
            self.config.l2_block_range_for_header_timestamp(first_batch.timestamp)
        else {
            return Err(BatchValidity::Drop(BatchDropReason::SpanBatchMisalignedTimestamp));
        };
        let block_count = span.batches.len() as u64;
        let eligible_starts = timestamp_range
            .clone()
            .filter(|start| {
                *start > self.config.genesis.l2.number
                    && *start <= next_block_number
                    && *start + block_count > next_block_number
            })
            .collect::<Vec<_>>();
        if eligible_starts.is_empty() {
            if *timestamp_range.start() > next_block_number {
                return Err(BatchValidity::Drop(BatchDropReason::FutureTimestampHolocene));
            }
            if *timestamp_range.end() + block_count <= next_block_number {
                return Err(BatchValidity::Past);
            }
            return Err(BatchValidity::Drop(BatchDropReason::SpanBatchMisalignedTimestamp));
        }

        let mut resolved_start = None;
        let mut provider_failed = false;
        for start in eligible_starts {
            let parent = if start == next_block_number {
                l2_safe_head
            } else {
                let parent_number = start - 1;
                match self.fetcher.l2_block_info_by_number(parent_number).await {
                    Ok(parent) => parent,
                    Err(error) => {
                        warn!(target: "batch_span", block_number = parent_number, error = %error, "Failed to fetch candidate span parent");
                        provider_failed = true;
                        continue;
                    }
                }
            };
            if span.parent_check.as_slice() == &parent.block_info.hash[..20]
                && resolved_start.replace(start).is_some()
            {
                return Err(BatchValidity::Drop(BatchDropReason::ParentHashMismatch));
            }
        }

        if provider_failed {
            return Err(BatchValidity::Undecided);
        }
        resolved_start.ok_or(BatchValidity::Drop(BatchDropReason::ParentHashMismatch))
    }
}

#[async_trait]
impl<P, BF> NextBatchProvider for BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Send + Debug,
    BF: L2ChainProvider + Send + Debug,
{
    fn flush(&mut self) {
        if self.is_active().unwrap_or(false) {
            self.prev.flush();
            self.span = None;
            self.pending_span = None;
            self.buffer.clear();
        }
    }

    fn span_buffer_size(&self) -> usize {
        self.buffer.len()
    }

    async fn next_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> PipelineResult<Batch> {
        // If the stage is not active, "pass" the next batch
        // through this stage to the BatchQueue stage.
        if !self.is_active()? {
            trace!(target: "batch_span", "BatchStream stage is inactive, pass-through.");
            return self.prev.next_batch().await;
        }

        // If the buffer is empty, attempt to pull a batch from the previous stage.
        if self.buffer.is_empty() {
            if self.pending_span.is_none() {
                let inclusion_block = self.origin().ok_or(PipelineError::MissingOrigin.crit())?;
                match self.prev.next_batch().await? {
                    Batch::Single(batch) => return Ok(Batch::Single(batch)),
                    Batch::Span(span) => self.pending_span = Some((span, inclusion_block)),
                }
            }

            let (mut span, inclusion_block) =
                self.pending_span.take().expect("span must be staged");
            if self.config.denim_activation_block_number().is_some() {
                let first_block_number = match self.resolve_span_start_block(&span, parent).await {
                    Ok(number) => number,
                    Err(validity) => {
                        Metrics::pipeline_batch_validity(validity.to_string()).increment(1.0);
                        match validity {
                            BatchValidity::Drop(_) => self.flush(),
                            BatchValidity::Past => {}
                            BatchValidity::Undecided | BatchValidity::Future => {
                                self.pending_span = Some((span, inclusion_block));
                            }
                            BatchValidity::Accept => {
                                unreachable!("resolution cannot accept directly")
                            }
                        }
                        return Err(PipelineError::NotEnoughData.temp());
                    }
                };
                span.apply_block_number_timestamps(self.config.as_ref(), first_block_number);
            }

            let (validity, _) = base_metrics::time!(Metrics::pipeline_check_batch_prefix(), {
                span.check_batch_prefix(
                    self.config.as_ref(),
                    l1_origins,
                    parent,
                    &inclusion_block,
                    &mut self.fetcher,
                )
                .await
            });
            Metrics::pipeline_batch_validity(validity.to_string()).increment(1.0);

            match validity {
                BatchValidity::Accept => self.span = Some(span),
                BatchValidity::Drop(_) => {
                    self.flush();
                    return Err(PipelineError::NotEnoughData.temp());
                }
                BatchValidity::Past => return Err(PipelineError::NotEnoughData.temp()),
                BatchValidity::Undecided | BatchValidity::Future => {
                    self.pending_span = Some((span, inclusion_block));
                    return Err(PipelineError::NotEnoughData.temp());
                }
            }
        }

        // Attempt to pull a SingleBatch out of the SpanBatch.
        match self.get_single_batch(parent, l1_origins) {
            Ok(Some(single_batch)) => Ok(Batch::Single(single_batch)),
            Ok(None) => Err(PipelineError::NotEnoughData.temp()),
            Err(e) => {
                warn!(target: "batch_span", error = %e, "Extracting singular batches from span batch failed");
                // If singular batch extraction fails, it should be handled the same as a
                // dropped batch during span batch prefix checks.
                self.flush();
                Err(PipelineError::NotEnoughData.temp())
            }
        }
    }
}

#[async_trait]
impl<P, BF> OriginAdvancer for BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Send + Debug,
    BF: L2ChainProvider + Send + Debug,
{
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        self.prev.advance_origin().await
    }
}

impl<P, BF> OriginProvider for BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Debug,
    BF: L2ChainProvider + Debug,
{
    fn origin(&self) -> Option<BlockInfo> {
        self.prev.origin()
    }
}

#[async_trait]
impl<P, BF> StageReset for BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Debug + Send,
    BF: L2ChainProvider + Send + Debug,
{
    async fn reset(
        &mut self,
        l1_origin: BlockNumHash,
        system_config: SystemConfig,
    ) -> PipelineResult<()> {
        self.prev.reset(l1_origin, system_config).await?;
        self.buffer.clear();
        self.span = None;
        self.pending_span = None;
        Ok(())
    }

    async fn activate(&mut self) -> PipelineResult<()> {
        self.prev.activate().await?;
        self.buffer.clear();
        self.span = None;
        self.pending_span = None;
        Ok(())
    }

    async fn flush_channel(&mut self) -> PipelineResult<()> {
        self.prev.flush_channel().await?;
        self.buffer.clear();
        self.span = None;
        self.pending_span = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_consensus::{BlockBody, Header};
    use alloy_eips::{BlockNumHash, NumHash};
    use alloy_primitives::{FixedBytes, b256};
    use base_common_consensus::BaseBlock;
    use base_common_genesis::{BaseUpgradeConfig, ChainGenesis, SystemConfig, UpgradeConfig};
    use base_protocol::{SingleBatch, SpanBatchElement};

    use super::*;
    use crate::{
        StageReset,
        test_utils::{TestBatchStreamProvider, TestL2ChainProvider},
    };

    fn denim_config() -> Arc<RollupConfig> {
        Arc::new(RollupConfig {
            block_time: 2,
            seq_window_size: 100,
            upgrades: UpgradeConfig {
                delta_time: Some(0),
                holocene_time: Some(0),
                base: BaseUpgradeConfig { denim: Some(6), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn span(timestamp: u64, block_count: usize, parent_hash: FixedBytes<32>) -> SpanBatch {
        SpanBatch {
            parent_check: FixedBytes::from_slice(&parent_hash[..20]),
            batches: vec![SpanBatchElement { timestamp, ..Default::default() }; block_count],
            ..Default::default()
        }
    }

    fn l2_block(number: u64, hash: FixedBytes<32>) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, hash, ..Default::default() },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn resolves_each_denim_same_second_start_slot() {
        for start_block in 3..=7 {
            let parent_hash = FixedBytes::repeat_byte(start_block as u8);
            let mut stream = BatchStream::new(
                TestBatchStreamProvider::new(vec![]),
                denim_config(),
                TestL2ChainProvider::default(),
            );

            assert_eq!(
                stream
                    .resolve_span_start_block(
                        &span(6, 1, parent_hash),
                        l2_block(start_block - 1, parent_hash),
                    )
                    .await,
                Ok(start_block)
            );
        }
    }

    #[tokio::test]
    async fn resolves_overlapping_denim_span_by_parent_hash() {
        let blocks =
            (2..=4).map(|number| l2_block(number, FixedBytes::repeat_byte(number as u8))).collect();
        let safe_head = l2_block(5, FixedBytes::repeat_byte(5));
        let provider = TestL2ChainProvider { blocks, ..Default::default() };
        let mut stream =
            BatchStream::new(TestBatchStreamProvider::new(vec![]), denim_config(), provider);

        assert_eq!(
            stream
                .resolve_span_start_block(&span(6, 5, FixedBytes::repeat_byte(2)), safe_head)
                .await,
            Ok(3)
        );
    }

    #[tokio::test]
    async fn drops_unmatched_or_ambiguous_denim_span_parent() {
        for parent_hashes in [
            [FixedBytes::repeat_byte(2), FixedBytes::repeat_byte(3)],
            [FixedBytes::repeat_byte(2), FixedBytes::repeat_byte(2)],
        ] {
            let blocks = vec![l2_block(2, parent_hashes[0]), l2_block(3, parent_hashes[1])];
            let safe_head = l2_block(4, FixedBytes::repeat_byte(4));
            let provider = TestL2ChainProvider { blocks, ..Default::default() };
            let mut stream =
                BatchStream::new(TestBatchStreamProvider::new(vec![]), denim_config(), provider);
            let expected_parent = if parent_hashes[0] == parent_hashes[1] {
                parent_hashes[0]
            } else {
                FixedBytes::repeat_byte(9)
            };

            assert_eq!(
                stream.resolve_span_start_block(&span(6, 3, expected_parent), safe_head).await,
                Err(BatchValidity::Drop(BatchDropReason::ParentHashMismatch))
            );
        }
    }

    #[tokio::test]
    async fn keeps_denim_span_undecided_when_candidate_parent_is_unavailable() {
        let safe_head = l2_block(4, FixedBytes::repeat_byte(4));
        let mut stream = BatchStream::new(
            TestBatchStreamProvider::new(vec![]),
            denim_config(),
            TestL2ChainProvider::default(),
        );

        assert_eq!(
            stream
                .resolve_span_start_block(&span(6, 3, FixedBytes::repeat_byte(2)), safe_head)
                .await,
            Err(BatchValidity::Undecided)
        );
    }

    #[tokio::test]
    async fn test_batch_stream_flush() {
        let config = Arc::new(RollupConfig {
            upgrades: UpgradeConfig { holocene_time: Some(0), ..Default::default() },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(vec![]);
        let mut stream = BatchStream::new(prev, config, TestL2ChainProvider::default());
        stream.buffer.push_back(SingleBatch::default());
        stream.span = Some(SpanBatch::default());
        assert!(!stream.buffer.is_empty());
        assert!(stream.span.is_some());
        stream.flush();
        assert!(stream.buffer.is_empty());
        assert!(stream.span.is_none());
    }

    #[tokio::test]
    async fn test_batch_stream_reset() {
        let config = Arc::new(RollupConfig {
            upgrades: UpgradeConfig { holocene_time: Some(0), ..Default::default() },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(vec![]);
        let mut stream =
            BatchStream::new(prev, Arc::clone(&config), TestL2ChainProvider::default());
        stream.buffer.push_back(SingleBatch::default());
        stream.span = Some(SpanBatch::default());
        assert!(!stream.prev.reset);
        stream.reset(BlockNumHash::default(), SystemConfig::default()).await.unwrap();
        assert!(stream.prev.reset);
        assert!(stream.buffer.is_empty());
        assert!(stream.span.is_none());
    }

    #[tokio::test]
    async fn test_batch_stream_flush_channel() {
        let config = Arc::new(RollupConfig {
            upgrades: UpgradeConfig { holocene_time: Some(0), ..Default::default() },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(vec![]);
        let mut stream =
            BatchStream::new(prev, Arc::clone(&config), TestL2ChainProvider::default());
        stream.buffer.push_back(SingleBatch::default());
        stream.span = Some(SpanBatch::default());
        assert!(!stream.prev.flushed);
        stream.flush_channel().await.unwrap();
        assert!(stream.prev.flushed);
        assert!(stream.buffer.is_empty());
        assert!(stream.span.is_none());
    }

    #[tokio::test]
    async fn test_batch_stream_inactive() {
        let (trace_store, _guard) = base_protocol::capture_traces!();

        let data = vec![Ok(Batch::Single(SingleBatch::default()))];
        let config = Arc::new(RollupConfig {
            upgrades: UpgradeConfig { holocene_time: Some(100), ..Default::default() },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(data);
        let mut stream =
            BatchStream::new(prev, Arc::clone(&config), TestL2ChainProvider::default());

        // The stage should not be active.
        assert!(!stream.is_active().unwrap());

        // The next batch should be passed through to the [BatchQueue] stage.
        let batch = stream.next_batch(Default::default(), &[]).await.unwrap();
        assert_eq!(batch, Batch::Single(SingleBatch::default()));

        let logs = trace_store.get_by_level(tracing::Level::TRACE);
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("BatchStream stage is inactive, pass-through."));
    }

    #[tokio::test]
    async fn test_span_buffer() {
        let mock_batch = SpanBatch {
            batches: vec![
                SpanBatchElement { epoch_num: 1, timestamp: 2, ..Default::default() },
                SpanBatchElement { epoch_num: 1, timestamp: 4, ..Default::default() },
            ],
            ..Default::default()
        };
        let mock_origins = [BlockInfo { number: 1, timestamp: 12, ..Default::default() }];

        let data = vec![Ok(Batch::Span(mock_batch.clone()))];
        let config = Arc::new(RollupConfig {
            block_time: 2,
            upgrades: UpgradeConfig {
                delta_time: Some(0),
                holocene_time: Some(0),
                ..Default::default()
            },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(data);
        let provider = TestL2ChainProvider::default();
        let mut stream = BatchStream::new(prev, Arc::clone(&config), provider);

        // The stage should be active.
        assert!(stream.is_active().unwrap());

        // The next batches should be single batches derived from the span batch.
        let batch = stream.next_batch(Default::default(), &mock_origins).await.unwrap();
        if let Batch::Single(single) = batch {
            assert_eq!(single.epoch_num, 1);
            assert_eq!(single.timestamp, 2);
        } else {
            panic!("Wrong batch type");
        }

        let batch = stream.next_batch(Default::default(), &mock_origins).await.unwrap();
        if let Batch::Single(single) = batch {
            assert_eq!(single.epoch_num, 1);
            assert_eq!(single.timestamp, 4);
        } else {
            panic!("Wrong batch type");
        }

        let err = stream.next_batch(Default::default(), &mock_origins).await.unwrap_err();
        assert_eq!(err, PipelineError::Eof.temp());
        assert_eq!(stream.span_buffer_size(), 0);
        assert!(stream.span.is_none());

        // Add more data into the provider, see if the buffer is re-hydrated.
        stream.prev.batches.push(Ok(Batch::Span(mock_batch.clone())));

        // The next batches should be single batches derived from the span batch.
        let batch = stream.next_batch(Default::default(), &mock_origins).await.unwrap();
        if let Batch::Single(single) = batch {
            assert_eq!(single.epoch_num, 1);
            assert_eq!(single.timestamp, 2);
        } else {
            panic!("Wrong batch type");
        }

        let batch = stream.next_batch(Default::default(), &mock_origins).await.unwrap();
        if let Batch::Single(single) = batch {
            assert_eq!(single.epoch_num, 1);
            assert_eq!(single.timestamp, 4);
        } else {
            panic!("Wrong batch type");
        }

        let err = stream.next_batch(Default::default(), &mock_origins).await.unwrap_err();
        assert_eq!(err, PipelineError::Eof.temp());
        assert_eq!(stream.span_buffer_size(), 0);
        assert!(stream.span.is_none());
    }

    #[tokio::test]
    async fn test_span_batch_extraction_error_flushes_stage() {
        let (trace_store, _guard) = base_protocol::capture_traces!();

        let parent_hash = b256!("1111111111111111111111111111111111111111000000000000000000000000");
        let l1_block_hash =
            b256!("3333333333333333333333333333333333333333000000000000000000000000");
        let config = Arc::new(RollupConfig {
            seq_window_size: 100,
            block_time: 10,
            upgrades: UpgradeConfig {
                delta_time: Some(0),
                holocene_time: Some(0),
                ..Default::default()
            },
            genesis: ChainGenesis {
                l2: BlockNumHash { number: 40, hash: parent_hash },
                ..Default::default()
            },
            ..Default::default()
        });

        let l1_block =
            BlockInfo { number: 10, timestamp: 5, hash: l1_block_hash, ..Default::default() };
        let l1_blocks = vec![l1_block];
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { number: 41, timestamp: 10, parent_hash, ..Default::default() },
            l1_origin: l1_block.id(),
            ..Default::default()
        };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                number: 40,
                hash: parent_hash,
                timestamp: 0,
                ..Default::default()
            },
            l1_origin: BlockNumHash { number: 9, ..Default::default() },
            ..Default::default()
        };
        let base_block = BaseBlock {
            header: Header { number: 41, ..Default::default() },
            body: BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
        };

        let span_batch = SpanBatch {
            batches: vec![
                SpanBatchElement { epoch_num: 9, timestamp: 10, ..Default::default() },
                SpanBatchElement { epoch_num: 9, timestamp: 20, ..Default::default() },
                SpanBatchElement { epoch_num: 10, timestamp: 30, ..Default::default() },
            ],
            parent_check: FixedBytes::<20>::from_slice(&parent_hash[..20]),
            l1_origin_check: FixedBytes::<20>::from_slice(&l1_block_hash[..20]),
            ..Default::default()
        };

        let mut prev = TestBatchStreamProvider::new(vec![Ok(Batch::Span(span_batch))]);
        prev.origin = Some(l1_block);

        let mut provider = TestL2ChainProvider::default();
        provider.blocks.push(l2_parent);
        provider.base_blocks.push(base_block);

        let mut stream = BatchStream::new(prev, config, provider);
        let err = stream.next_batch(l2_safe_head, &l1_blocks).await.unwrap_err();

        assert_eq!(err, PipelineError::NotEnoughData.temp());
        assert!(stream.span.is_none());
        assert_eq!(stream.span_buffer_size(), 0);

        let logs = trace_store.get_by_level(tracing::Level::WARN);
        assert_eq!(logs.len(), 1);
        assert!(
            logs[0].contains("Extracting singular batches from span batch failed")
                && logs[0].contains("error")
                && logs[0].contains("Future batch L1 origin before safe head")
        );
    }

    #[tokio::test]
    async fn test_single_batch_pass_through() {
        let data = vec![Ok(Batch::Single(SingleBatch::default()))];
        let config = Arc::new(RollupConfig {
            upgrades: UpgradeConfig { holocene_time: Some(0), ..Default::default() },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(data);
        let mut stream =
            BatchStream::new(prev, Arc::clone(&config), TestL2ChainProvider::default());

        // The stage should be active.
        assert!(stream.is_active().unwrap());

        // The next batch should be passed through to the [BatchQueue] stage.
        let batch = stream.next_batch(Default::default(), &[]).await.unwrap();
        assert!(matches!(batch, Batch::Single(_)));
        assert_eq!(stream.span_buffer_size(), 0);
        assert!(stream.span.is_none());
    }

    #[tokio::test]
    async fn test_past_span_batch() {
        let mock_batch = SpanBatch {
            batches: vec![
                SpanBatchElement { epoch_num: 1, timestamp: 2, ..Default::default() },
                SpanBatchElement { epoch_num: 1, timestamp: 4, ..Default::default() },
            ],
            ..Default::default()
        };
        let mock_origins = [BlockInfo { number: 1, timestamp: 12, ..Default::default() }];
        let data = vec![Ok(Batch::Span(mock_batch))];

        let config = Arc::new(RollupConfig {
            upgrades: UpgradeConfig { holocene_time: Some(0), ..Default::default() },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(data);
        let mut stream =
            BatchStream::new(prev, Arc::clone(&config), TestL2ChainProvider::default());

        // The stage should be active.
        assert!(stream.is_active().unwrap());

        let parent = L2BlockInfo {
            block_info: BlockInfo { number: 10, timestamp: 100, ..Default::default() },
            l1_origin: NumHash::default(),
            seq_num: 0,
        };

        // `next_batch` should return an error if the span batch is in the past.
        let err = stream.next_batch(parent, &mock_origins).await.unwrap_err();
        assert_eq!(err, PipelineError::NotEnoughData.temp());
    }
}
