//! This module contains the `BatchStream` stage.

use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use core::fmt::Debug;

use alloy_eips::BlockNumHash;
use async_trait::async_trait;
use base_common_chain_config::{RollupConfig, SystemConfig};
use base_consensus_batch::{
    Batch, BatchValidity, BatchWithInclusionBlock, BlockInfo, L2BlockInfo, SingleBatch, SpanBatch,
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
/// It slots in between the [`ChannelReader`] and [`BatchValidator`]
/// stages, buffering span batches until they are validated.
///
/// [`Holocene`]: https://specs.base.org/upgrades/holocene/overview
/// [`ChannelReader`]: crate::stages::ChannelReader
/// [`BatchValidator`]: crate::stages::BatchValidator
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
        Self { prev, span: None, buffer: VecDeque::new(), config, fetcher }
    }

    /// Gets a span-derived [`SingleBatch`] from the in-memory buffer and assigns its parent hash.
    pub fn get_single_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> Result<Option<SingleBatch>, SpanBatchError> {
        trace!(target: "batch_span", buffer_len = self.buffer.len(), "Attempting to get a SingleBatch from buffer");

        let parent_hash = parent.block_info.hash;
        self.try_hydrate_buffer(parent, l1_origins)?;
        Ok(self.buffer.pop_front().map(|mut batch| {
            batch.parent_hash = parent_hash;
            batch
        }))
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
        Ok(())
    }
}

#[async_trait]
impl<P, BF> NextBatchProvider for BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Send + Debug,
    BF: L2ChainProvider + Send + Debug,
{
    fn flush(&mut self) {
        self.prev.flush();
        self.span = None;
        self.buffer.clear();
    }

    async fn next_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> PipelineResult<SingleBatch> {
        let next = parent.block_info.number + 1;
        if self.config.is_cobalt_active(self.config.l2_block_timestamp(next))
            && (self.span.is_some() || !self.buffer.is_empty())
        {
            warn!(target: "batch_span", next_block_number = next, "Dropping cached span state after Cobalt activation");
            self.flush();
            return Err(PipelineError::NotEnoughData.temp());
        }

        // If the buffer is empty, attempt to pull a batch from the previous stage.
        if self.buffer.is_empty() {
            // Safety: bubble up any errors from the batch reader.
            let batch_with_inclusion = BatchWithInclusionBlock::new(
                self.origin().ok_or(PipelineError::MissingOrigin.crit())?,
                self.prev.next_batch().await?,
            );

            // If the next batch is a singular batch, it is immediately
            // forwarded to the `BatchValidator` stage. Otherwise, we buffer
            // the span batch in this stage if it passes the validity checks.
            match batch_with_inclusion.batch {
                Batch::Single(b) => return Ok(b),
                Batch::Span(b) => {
                    let (validity, _) = base_common_observability_metrics::time!(
                        Metrics::pipeline_span_prefix_validation_duration_seconds(),
                        {
                            b.check_batch_prefix(
                                self.config.as_ref(),
                                l1_origins,
                                parent,
                                &batch_with_inclusion.inclusion_block,
                                &mut self.fetcher,
                            )
                            .await
                        }
                    );

                    match validity {
                        BatchValidity::Accept => self.span = Some(*b),
                        BatchValidity::Drop(_) => {
                            // Flush the stage.
                            self.flush();

                            return Err(PipelineError::NotEnoughData.temp());
                        }
                        BatchValidity::Past | BatchValidity::Undecided | BatchValidity::Future => {
                            return Err(PipelineError::NotEnoughData.temp());
                        }
                    }
                }
            }
        }

        // Attempt to pull a SingleBatch out of the SpanBatch.
        match self.get_single_batch(parent, l1_origins) {
            Ok(Some(single_batch)) => Ok(single_batch),
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
        Ok(())
    }

    async fn activate(&mut self) -> PipelineResult<()> {
        self.prev.activate().await?;
        self.buffer.clear();
        self.span = None;
        Ok(())
    }

    async fn flush_channel(&mut self) -> PipelineResult<()> {
        self.prev.flush_channel().await?;
        self.buffer.clear();
        self.span = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ChannelReader,
        test_utils::{TestChannelReaderProvider, TestL2ChainProvider},
    };
    use alloc::{vec, vec::Vec};
    use alloy_primitives::{B256, Bytes};
    use alloy_rlp::Encodable;
    use base_common_chain_config::{BaseUpgradeConfig, UpgradeConfig};
    use base_consensus_batch::{BatchReader, SpanBatchElement};

    /// Real channel decoding and span validation must produce consecutive singular batches.
    #[tokio::test]
    async fn channel_span_derives_blocks_and_preserves_parent_hashes() {
        let config = Arc::new(RollupConfig {
            block_time: 2,
            upgrades: UpgradeConfig {
                delta_time: Some(0),
                holocene_time: Some(0),
                ..Default::default()
            },
            ..Default::default()
        });
        let origins = [BlockInfo { number: 1, ..Default::default() }];
        let mut span = SpanBatch::default();
        for timestamp in [2, 4] {
            span.append_singular_batch(
                SingleBatch { epoch_num: 1, timestamp, ..Default::default() },
                0,
            )
            .unwrap();
        }
        let mut encoded = Vec::new();
        Batch::Span(Box::new(span)).encode(&mut encoded).unwrap();
        let mut data = Vec::new();
        Bytes::from(encoded).encode(&mut data);
        let provider = TestChannelReaderProvider::new(vec![]);
        let mut reader = ChannelReader::new(provider, Arc::clone(&config));
        let mut batches = BatchReader::new(Vec::new(), 10_000_000);
        batches.decompressed = data;
        reader.next_batch = Some(batches);
        let mut stream = BatchStream::new(reader, config, TestL2ChainProvider::default());
        let first = stream.next_batch(L2BlockInfo::default(), &origins).await.unwrap();
        assert_eq!(first.timestamp, 2);
        assert_eq!(first.epoch_num, 1);
        let parent = L2BlockInfo {
            block_info: BlockInfo {
                number: 1,
                timestamp: 2,
                hash: B256::repeat_byte(42),
                ..Default::default()
            },
            l1_origin: origins[0].id(),
            ..Default::default()
        };
        let second = stream.next_batch(parent, &origins).await.unwrap();
        assert_eq!(second.timestamp, 4);
        assert_eq!(second.parent_hash, parent.block_info.hash);
        assert_eq!(second.epoch_hash, origins[0].hash);
    }

    #[tokio::test]
    async fn cached_span_stops_at_cobalt_and_flushes_on_reset() {
        let config = Arc::new(RollupConfig {
            block_time: 2,
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { cobalt: Some(4), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        });
        let reader =
            ChannelReader::new(TestChannelReaderProvider::new(vec![]), Arc::clone(&config));
        let mut stream = BatchStream::new(reader, config, TestL2ChainProvider::default());
        stream.buffer.push_back(SingleBatch { timestamp: 4, ..Default::default() });
        let parent = L2BlockInfo {
            block_info: BlockInfo { number: 1, timestamp: 2, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(stream.next_batch(parent, &[]).await, Err(PipelineError::NotEnoughData.temp()));
        assert!(stream.buffer.is_empty());
        stream.span =
            Some(SpanBatch { batches: vec![SpanBatchElement::default()], ..Default::default() });
        stream.reset(BlockNumHash::default(), SystemConfig::default()).await.unwrap();
        assert!(stream.span.is_none());
    }
}
