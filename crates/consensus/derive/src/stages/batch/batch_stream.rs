//! This module contains the `BatchStream` stage.

use alloc::{boxed::Box, collections::VecDeque, string::ToString, sync::Arc};
use core::fmt::Debug;

use alloy_eips::BlockNumHash;
use async_trait::async_trait;
use base_common_consensus::BlsConstants;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_protocol::{
    Batch, BatchValidity, BatchWithInclusionBlock, BlockInfo, BlsChunkBatch, L2BlockInfo,
    SingleBatch, SpanBatch, SpanBatchError,
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
/// On the native BLS development chain it is also the only stage that releases native BLS
/// transfers: it authenticates each [`BlsChunkBatch`] as a whole before releasing any member,
/// and drops every other batch carrying a transfer.
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
    /// A buffer of single batches derived from the [`SpanBatch`].
    pub buffer: VecDeque<SingleBatch>,
    /// A chunk read from the previous stage whose verdict is not known yet. It is retried
    /// unchanged after a provider error, so an unavailable verdict never skips a chunk.
    pub pending_chunk: Option<BlsChunkBatch>,
    /// Members of an authenticated chunk awaiting release. Tracked apart from span state, so the
    /// Denim span flush rule does not drop them.
    pub chunk_buffer: VecDeque<SingleBatch>,
    /// Whether the batch most recently returned came from an authenticated chunk.
    pub bls_authenticated: bool,
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
        Self {
            prev,
            span: None,
            buffer: VecDeque::new(),
            pending_chunk: None,
            chunk_buffer: VecDeque::new(),
            bls_authenticated: false,
            config,
            fetcher,
        }
    }

    /// Returns if the [`BatchStream`] stage is active based on the
    /// origin timestamp and holocene activation timestamp.
    pub fn is_active(&self) -> PipelineResult<bool> {
        let origin = self.prev.origin().ok_or(PipelineError::MissingOrigin.crit())?;
        Ok(self.config.is_holocene_active(origin.timestamp))
    }

    /// Returns whether `batch` carries a native BLS transfer on the chain that defines them.
    ///
    /// Such a batch is only valid as a member of an authenticated chunk. Other chains never
    /// define the type, so their batches are left to the existing rules unchanged.
    pub fn carries_bls_transfer(&self, batch: &SingleBatch) -> bool {
        self.config.l2_chain_id.id() == BlsConstants::CHAIN_ID
            && BlsChunkBatch::count_transfers(&batch.transactions) > 0
    }

    /// Drops all buffered batch state, including unreleased chunk members.
    pub fn clear(&mut self) {
        self.span = None;
        self.buffer.clear();
        self.pending_chunk = None;
        self.chunk_buffer.clear();
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
        let batch_count = self.buffer.len() as f64;
        Metrics::pipeline_batch_buffer().set(batch_count);
        let batch_size = core::mem::size_of_val(&self.buffer) as f64;
        Metrics::pipeline_batch_mem().set(batch_size);
        Ok(())
    }
}

impl<P, BF> BatchStream<P, BF>
where
    P: BatchStreamProvider + OriginAdvancer + OriginProvider + StageReset + Send + Debug,
    BF: L2ChainProvider + Send + Debug,
{
    /// Authenticates `chunk` on top of `parent` and releases its first unsafe member.
    ///
    /// The chunk is located against the safe chain, then the whole chunk, including members that
    /// are already safe, is verified at its first parent before any member is buffered. An
    /// invalid chunk is dropped with the rest of its channel. A provider error keeps the chunk
    /// pending so the next call retries it.
    pub async fn release_chunk(
        &mut self,
        chunk: BlsChunkBatch,
        parent: L2BlockInfo,
    ) -> PipelineResult<Batch> {
        if self.config.l2_chain_id.id() != BlsConstants::CHAIN_ID || chunk.validate().is_err() {
            warn!(target: "batch_stream", "Dropping unsupported or malformed BLS chunk");
            self.flush();
            return Err(PipelineError::NotEnoughData.temp());
        }

        let (validity, overlap) =
            match chunk.check_prefix(self.config.as_ref(), parent, &mut self.fetcher).await {
                Ok(result) => result,
                Err(error) => {
                    self.pending_chunk = Some(chunk);
                    return Err(BF::provider_error_into_pipeline_error(error));
                }
            };
        Metrics::pipeline_batch_validity(validity.to_string()).increment(1.0);
        match validity {
            BatchValidity::Accept => {}
            BatchValidity::Drop(reason) => {
                warn!(target: "batch_stream", reason = %reason, "Dropping BLS chunk, flushing current channel");
                self.flush();
                return Err(PipelineError::NotEnoughData.temp());
            }
            BatchValidity::Past => {
                debug!(target: "batch_stream", "Dropping BLS chunk that is already safe");
                return Err(PipelineError::NotEnoughData.temp());
            }
            BatchValidity::Undecided | BatchValidity::Future => {
                self.pending_chunk = Some(chunk);
                return Err(PipelineError::NotEnoughData.temp());
            }
        }

        // `validate` guarantees an aggregate exactly when the chunk carries transfers.
        if chunk.transfer_count() > 0 {
            let verdict = self
                .fetcher
                .verify_bls_chunk(
                    chunk.batches[0].parent_hash,
                    &chunk.block_transactions(),
                    chunk.aggregate,
                )
                .await;
            match verdict {
                Ok(true) => {}
                Ok(false) => {
                    warn!(target: "batch_stream", members = chunk.batches.len(), "BLS chunk aggregate is invalid, flushing current channel");
                    self.flush();
                    return Err(PipelineError::NotEnoughData.temp());
                }
                Err(error) => {
                    warn!(target: "batch_stream", error = %error, "BLS chunk verdict unavailable, retrying");
                    self.pending_chunk = Some(chunk);
                    return Err(error);
                }
            }
        }

        self.chunk_buffer.extend(chunk.batches.into_iter().skip(overlap));
        let member = self.chunk_buffer.pop_front().ok_or(PipelineError::NotEnoughData.temp())?;
        self.bls_authenticated = true;
        Ok(Batch::Single(member))
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
            self.clear();
        }
    }

    fn span_buffer_size(&self) -> usize {
        self.buffer.len() + self.chunk_buffer.len()
    }

    fn is_bls_authenticated(&self) -> bool {
        self.bls_authenticated
    }

    async fn next_batch(
        &mut self,
        parent: L2BlockInfo,
        l1_origins: &[BlockInfo],
    ) -> PipelineResult<Batch> {
        self.bls_authenticated = false;

        // If the stage is not active, "pass" the next batch
        // through this stage to the BatchQueue stage. Chunks and native BLS transfers are only
        // accepted by the active stage.
        if !self.is_active()? {
            trace!(target: "batch_span", "BatchStream stage is inactive, pass-through.");
            return match self.prev.next_batch().await? {
                Batch::BlsChunk(_) => {
                    warn!(target: "batch_stream", "Dropping BLS chunk before Holocene");
                    Err(PipelineError::NotEnoughData.temp())
                }
                Batch::Single(b) if self.carries_bls_transfer(&b) => {
                    warn!(target: "batch_stream", "Dropping single batch with an unauthenticated BLS transfer");
                    Err(PipelineError::NotEnoughData.temp())
                }
                batch => Ok(batch),
            };
        }

        if let Some(member) = self.chunk_buffer.pop_front() {
            self.bls_authenticated = true;
            return Ok(Batch::Single(member));
        }

        let next = parent.block_info.number + 1;
        if self.config.is_denim_active(self.config.l2_block_timestamp(next))
            && (self.span.is_some() || !self.buffer.is_empty())
        {
            warn!(target: "batch_span", next_block_number = next, "Dropping cached span state after Denim activation");
            self.flush();
            return Err(PipelineError::NotEnoughData.temp());
        }

        // If the buffer is empty, attempt to pull a batch from the previous stage.
        if self.buffer.is_empty() {
            let origin = self.origin().ok_or(PipelineError::MissingOrigin.crit())?;
            let batch = match self.pending_chunk.take() {
                Some(chunk) => Batch::BlsChunk(chunk),
                // Safety: bubble up any errors from the batch reader.
                None => self.prev.next_batch().await?,
            };
            let batch_with_inclusion = BatchWithInclusionBlock::new(origin, batch);

            // If the next batch is a singular batch, it is immediately
            // forwarded to the `BatchQueue` stage. Otherwise, we buffer
            // the span batch in this stage if it passes the validity checks.
            match batch_with_inclusion.batch {
                Batch::Single(b) if self.carries_bls_transfer(&b) => {
                    warn!(target: "batch_stream", "Dropping single batch with an unauthenticated BLS transfer, flushing current channel");
                    self.flush();
                    return Err(PipelineError::NotEnoughData.temp());
                }
                Batch::Single(b) => return Ok(Batch::Single(b)),
                Batch::BlsChunk(chunk) => return self.release_chunk(chunk, parent).await,
                Batch::Span(b) => {
                    let (validity, _) =
                        base_metrics::time!(Metrics::pipeline_check_batch_prefix(), {
                            b.check_batch_prefix(
                                self.config.as_ref(),
                                l1_origins,
                                parent,
                                &batch_with_inclusion.inclusion_block,
                                &mut self.fetcher,
                            )
                            .await
                        });
                    Metrics::pipeline_batch_validity(validity.to_string()).increment(1.0);

                    match validity {
                        BatchValidity::Accept => self.span = Some(b),
                        BatchValidity::Drop(_) => {
                            // Flush the stage.
                            self.flush();

                            return Err(PipelineError::NotEnoughData.temp());
                        }
                        BatchValidity::Past => {
                            if !self.is_active()? {
                                error!(target: "batch_stream", "BatchValidity::Past is not allowed pre-holocene");
                                return Err(PipelineError::InvalidBatchValidity.crit());
                            }

                            return Err(PipelineError::NotEnoughData.temp());
                        }
                        BatchValidity::Undecided | BatchValidity::Future => {
                            return Err(PipelineError::NotEnoughData.temp());
                        }
                    }
                }
            }
        }

        // Attempt to pull a SingleBatch out of the SpanBatch.
        match self.get_single_batch(parent, l1_origins) {
            Ok(Some(single_batch)) if self.carries_bls_transfer(&single_batch) => {
                warn!(target: "batch_span", "Span-derived batch carries a BLS transfer, flushing current channel");
                self.flush();
                Err(PipelineError::NotEnoughData.temp())
            }
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
        self.clear();
        Ok(())
    }

    async fn activate(&mut self) -> PipelineResult<()> {
        self.prev.activate().await?;
        self.clear();
        Ok(())
    }

    async fn flush_channel(&mut self) -> PipelineResult<()> {
        self.prev.flush_channel().await?;
        self.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxEip1559};
    use alloy_eips::{
        BlockNumHash, NumHash,
        eip2718::{Decodable2718, Encodable2718},
    };
    use alloy_primitives::{Address, B256, Bytes, FixedBytes, Sealable, Signature, U256, b256};
    use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxBlsTransfer, TxDeposit};
    use base_common_genesis::{BaseUpgradeConfig, ChainGenesis, SystemConfig, UpgradeConfig};
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx, SingleBatch, SpanBatchElement};

    use super::*;
    use crate::{
        PipelineErrorKind, StageReset,
        test_utils::{TestBatchStreamProvider, TestL2ChainProvider},
    };

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

        let parent = L2BlockInfo {
            block_info: BlockInfo { hash: FixedBytes::repeat_byte(0x11), ..Default::default() },
            ..Default::default()
        };
        let batch = stream.next_batch(parent, &mock_origins).await.unwrap();
        if let Batch::Single(single) = batch {
            assert_eq!(single.epoch_num, 1);
            assert_eq!(single.timestamp, 4);
            assert_eq!(single.parent_hash, parent.block_info.hash);
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
    async fn test_crossing_span_stops_before_first_denim_block() {
        let span = SpanBatch {
            batches: vec![
                SpanBatchElement { epoch_num: 1, timestamp: 2, ..Default::default() },
                SpanBatchElement { epoch_num: 1, timestamp: 4, ..Default::default() },
                SpanBatchElement { epoch_num: 1, timestamp: 6, ..Default::default() },
                SpanBatchElement { epoch_num: 1, timestamp: 8, ..Default::default() },
            ],
            ..Default::default()
        };
        let origins = [BlockInfo { number: 1, ..Default::default() }];
        let config = Arc::new(RollupConfig {
            block_time: 2,
            upgrades: UpgradeConfig {
                delta_time: Some(0),
                holocene_time: Some(0),
                base: BaseUpgradeConfig { denim: Some(6), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        });
        let prev = TestBatchStreamProvider::new(vec![Ok(Batch::Span(span))]);
        let mut stream = BatchStream::new(prev, config, TestL2ChainProvider::default());

        let first = stream.next_batch(L2BlockInfo::default(), &origins).await.unwrap();
        assert_eq!(first.timestamp(), 2);

        let second_parent = L2BlockInfo {
            block_info: BlockInfo { number: 1, timestamp: 2, ..Default::default() },
            ..Default::default()
        };
        let second = stream.next_batch(second_parent, &origins).await.unwrap();
        assert_eq!(second.timestamp(), 4);

        let denim_parent = L2BlockInfo {
            block_info: BlockInfo { number: 2, timestamp: 4, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(
            stream.next_batch(denim_parent, &origins).await,
            Err(PipelineError::NotEnoughData.temp())
        );
        assert!(stream.buffer.is_empty());
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
        let single =
            SingleBatch { parent_hash: FixedBytes::repeat_byte(0x11), ..Default::default() };
        let data = vec![Ok(Batch::Single(single.clone()))];
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
        assert_eq!(batch, Batch::Single(single));
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

    fn dev_config(denim: Option<u64>) -> Arc<RollupConfig> {
        Arc::new(RollupConfig {
            l2_chain_id: BlsConstants::CHAIN_ID.into(),
            block_time: 2,
            upgrades: UpgradeConfig {
                holocene_time: Some(0),
                base: BaseUpgradeConfig { denim, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn transfer(sender: u8, nonce: u64) -> Bytes {
        BaseTxEnvelope::BlsTransfer(
            TxBlsTransfer {
                chain_id: BlsConstants::CHAIN_ID,
                sender: Address::repeat_byte(sender),
                nonce,
                recipient: Address::repeat_byte(0xee),
                value: U256::from(1u64),
                max_priority_fee_per_gas: 1,
                max_fee_per_gas: 2,
            }
            .seal_slow(),
        )
        .encoded_2718()
        .into()
    }

    fn ordinary(nonce: u64) -> Bytes {
        BaseTxEnvelope::Eip1559(
            TxEip1559 { chain_id: BlsConstants::CHAIN_ID, nonce, ..Default::default() }
                .into_signed(Signature::test_signature()),
        )
        .encoded_2718()
        .into()
    }

    /// A chunk whose members build on the default safe head, with transfers from repeated
    /// senders in several members.
    fn chunk() -> BlsChunkBatch {
        BlsChunkBatch {
            aggregate: Some(FixedBytes::repeat_byte(0xa1)),
            batches: vec![
                SingleBatch {
                    transactions: vec![transfer(1, 0), ordinary(0)],
                    ..Default::default()
                },
                SingleBatch { transactions: vec![], ..Default::default() },
                SingleBatch {
                    transactions: vec![transfer(1, 1), transfer(2, 0)],
                    ..Default::default()
                },
            ],
        }
    }

    fn stream(
        cfg: Arc<RollupConfig>,
        batches: Vec<PipelineResult<Batch>>,
        verdicts: Vec<Option<bool>>,
    ) -> BatchStream<TestBatchStreamProvider, TestL2ChainProvider> {
        let provider =
            TestL2ChainProvider { bls_chunk_verdicts: verdicts.into(), ..Default::default() };
        BatchStream::new(TestBatchStreamProvider::new(batches), cfg, provider)
    }

    #[tokio::test]
    async fn test_bls_chunk_released_only_after_verification() {
        let chunk = chunk();
        let mut stream =
            stream(dev_config(None), vec![Ok(Batch::BlsChunk(chunk.clone()))], vec![Some(true)]);

        for member in &chunk.batches {
            let batch = stream.next_batch(L2BlockInfo::default(), &[]).await.unwrap();
            assert_eq!(batch, Batch::Single(member.clone()));
            assert!(stream.is_bls_authenticated());
        }
        // The whole chunk was verified once, at its first parent, before the first release.
        assert_eq!(
            stream.fetcher.bls_chunk_requests,
            vec![(B256::ZERO, chunk.block_transactions(), chunk.aggregate)]
        );
        assert_eq!(
            stream.next_batch(L2BlockInfo::default(), &[]).await,
            Err(PipelineError::Eof.temp())
        );
        assert!(!stream.is_bls_authenticated());
    }

    /// Partial and full chunks of every supported size release every member in order after one
    /// verification, with transfers from the same sender spread across members.
    #[tokio::test]
    async fn test_bls_chunk_sizes_release_all_members_in_order() {
        for size in [1, 5, 32, 64, 128] {
            let batches = (0..size as u64)
                .map(|index| SingleBatch {
                    timestamp: index,
                    transactions: vec![transfer(1, index), ordinary(index)],
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let chunk = BlsChunkBatch { aggregate: Some(FixedBytes::repeat_byte(0xa1)), batches };
            let mut stream = stream(
                dev_config(None),
                vec![Ok(Batch::BlsChunk(chunk.clone()))],
                vec![Some(true)],
            );
            for member in &chunk.batches {
                assert_eq!(
                    stream.next_batch(L2BlockInfo::default(), &[]).await,
                    Ok(Batch::Single(member.clone()))
                );
                assert!(stream.is_bls_authenticated());
            }
            assert_eq!(stream.fetcher.bls_chunk_requests.len(), 1, "size {size}");
            assert_eq!(stream.fetcher.bls_chunk_requests[0].1.len(), size);
        }
    }

    #[tokio::test]
    async fn test_invalid_bls_chunk_is_dropped_with_its_channel() {
        let mut stream =
            stream(dev_config(None), vec![Ok(Batch::BlsChunk(chunk()))], vec![Some(false)]);
        assert_eq!(
            stream.next_batch(L2BlockInfo::default(), &[]).await,
            Err(PipelineError::NotEnoughData.temp())
        );
        assert!(stream.prev.channel_flushed);
        assert!(stream.chunk_buffer.is_empty() && stream.pending_chunk.is_none());
        assert!(!stream.is_bls_authenticated());
    }

    #[tokio::test]
    async fn test_unavailable_bls_verdict_retries_the_same_chunk() {
        let chunk = chunk();
        let next = SingleBatch { timestamp: 9, ..Default::default() };
        // The provider pops from the back: the chunk first, then an ordinary batch.
        let batches = vec![Ok(Batch::Single(next)), Ok(Batch::BlsChunk(chunk.clone()))];
        let mut stream = stream(dev_config(None), batches, vec![None, Some(true)]);

        let err = stream.next_batch(L2BlockInfo::default(), &[]).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Temporary(PipelineError::Provider(_))));
        assert_eq!(stream.pending_chunk.as_ref(), Some(&chunk));
        assert!(!stream.prev.channel_flushed);

        let batch = stream.next_batch(L2BlockInfo::default(), &[]).await.unwrap();
        assert_eq!(batch, Batch::Single(chunk.batches[0].clone()));
        assert_eq!(stream.fetcher.bls_chunk_requests.len(), 2);
        assert_eq!(stream.fetcher.bls_chunk_requests[0], stream.fetcher.bls_chunk_requests[1]);
    }

    #[tokio::test]
    async fn test_unsupported_bls_verification_never_releases() {
        let mut stream = stream(dev_config(None), vec![Ok(Batch::BlsChunk(chunk()))], vec![]);
        for _ in 0..2 {
            assert_eq!(
                stream.next_batch(L2BlockInfo::default(), &[]).await,
                Err(PipelineError::BlsChunkVerificationUnsupported.crit())
            );
            assert!(stream.chunk_buffer.is_empty() && stream.pending_chunk.is_some());
        }
    }

    #[tokio::test]
    async fn test_bls_chunk_without_transfers_needs_no_verdict() {
        let chunk = BlsChunkBatch {
            aggregate: None,
            batches: vec![SingleBatch { transactions: vec![ordinary(0)], ..Default::default() }],
        };
        let mut stream = stream(dev_config(None), vec![Ok(Batch::BlsChunk(chunk.clone()))], vec![]);
        let batch = stream.next_batch(L2BlockInfo::default(), &[]).await.unwrap();
        assert_eq!(batch, Batch::Single(chunk.batches[0].clone()));
        assert!(stream.fetcher.bls_chunk_requests.is_empty());
    }

    #[tokio::test]
    async fn test_malformed_bls_chunk_is_dropped_before_verification() {
        let mut oversized = chunk();
        oversized.batches.resize(BlsChunkBatch::MAX_BLOCKS + 1, SingleBatch::default());
        let mut missing_witness = chunk();
        missing_witness.aggregate = None;
        for chunk in [oversized, missing_witness] {
            let mut stream =
                stream(dev_config(None), vec![Ok(Batch::BlsChunk(chunk))], vec![Some(true)]);
            assert_eq!(
                stream.next_batch(L2BlockInfo::default(), &[]).await,
                Err(PipelineError::NotEnoughData.temp())
            );
            assert!(stream.prev.channel_flushed);
            assert!(stream.fetcher.bls_chunk_requests.is_empty());
        }
    }

    #[tokio::test]
    async fn test_plain_single_with_bls_transfer_is_rejected_on_dev_chain_only() {
        let single = SingleBatch { transactions: vec![transfer(1, 0)], ..Default::default() };

        let mut dev = stream(dev_config(None), vec![Ok(Batch::Single(single.clone()))], vec![]);
        assert_eq!(
            dev.next_batch(L2BlockInfo::default(), &[]).await,
            Err(PipelineError::NotEnoughData.temp())
        );
        assert!(dev.prev.channel_flushed);

        // Other chains never define the type; their batches pass through unchanged.
        let other =
            Arc::new(RollupConfig { l2_chain_id: 8453.into(), ..(*dev_config(None)).clone() });
        let mut other = stream(other, vec![Ok(Batch::Single(single.clone()))], vec![]);
        assert_eq!(other.next_batch(L2BlockInfo::default(), &[]).await, Ok(Batch::Single(single)));
        assert!(!other.is_bls_authenticated());
    }

    #[tokio::test]
    async fn test_bls_chunk_and_transfers_dropped_before_holocene() {
        // Hold a subscriber so this test's pass-through trace cannot race the interest cache
        // `test_batch_stream_inactive` asserts on.
        let (_trace_store, _guard) = base_protocol::capture_traces!();
        let cfg = Arc::new(RollupConfig {
            upgrades: UpgradeConfig { holocene_time: Some(100), ..Default::default() },
            ..(*dev_config(None)).clone()
        });
        let single = SingleBatch { transactions: vec![transfer(1, 0)], ..Default::default() };
        let batches = vec![Ok(Batch::Single(single)), Ok(Batch::BlsChunk(chunk()))];
        let mut stream = stream(cfg, batches, vec![Some(true)]);
        for _ in 0..2 {
            assert_eq!(
                stream.next_batch(L2BlockInfo::default(), &[]).await,
                Err(PipelineError::NotEnoughData.temp())
            );
        }
        assert!(stream.fetcher.bls_chunk_requests.is_empty());
    }

    #[tokio::test]
    async fn test_bls_chunk_buffer_survives_denim_span_flush() {
        let chunk = chunk();
        let mut stream =
            stream(dev_config(Some(0)), vec![Ok(Batch::BlsChunk(chunk.clone()))], vec![Some(true)]);
        for member in &chunk.batches {
            assert_eq!(
                stream.next_batch(L2BlockInfo::default(), &[]).await,
                Ok(Batch::Single(member.clone()))
            );
        }
    }

    #[tokio::test]
    async fn test_reset_and_flush_drop_unreleased_chunk_members() {
        let mut stream =
            stream(dev_config(None), vec![Ok(Batch::BlsChunk(chunk()))], vec![Some(true)]);
        stream.next_batch(L2BlockInfo::default(), &[]).await.unwrap();
        assert_eq!(stream.span_buffer_size(), 2);
        stream.flush_channel().await.unwrap();
        assert_eq!(stream.span_buffer_size(), 0);

        stream.pending_chunk = Some(chunk());
        stream.chunk_buffer.push_back(SingleBatch::default());
        stream.reset(BlockNumHash::default(), SystemConfig::default()).await.unwrap();
        assert!(stream.pending_chunk.is_none() && stream.chunk_buffer.is_empty());

        stream.chunk_buffer.push_back(SingleBatch::default());
        stream.flush();
        assert!(stream.chunk_buffer.is_empty());
    }

    /// Canonical blocks `0..=4` and chunk members for blocks `1..=4`.
    fn chain(cfg: &mut RollupConfig) -> (TestL2ChainProvider, Vec<SingleBatch>) {
        let mut provider = TestL2ChainProvider::default();
        let mut members = Vec::new();
        let mut parent_hash = B256::ZERO;
        let epoch = BlockNumHash { number: 7, hash: B256::repeat_byte(7) };
        let info =
            L1BlockInfoBedrock::new(7, 0, 0, epoch.hash, 0, Address::ZERO, U256::ZERO, U256::ZERO);
        for number in 0..=4u64 {
            let deposit = BaseTxEnvelope::Deposit(
                TxDeposit {
                    input: L1BlockInfoTx::Bedrock(info).encode_calldata(),
                    ..Default::default()
                }
                .seal_slow(),
            );
            let user = vec![transfer(1, number), ordinary(number)];
            let mut transactions = vec![deposit];
            transactions.extend(
                user.iter().map(|raw| BaseTxEnvelope::decode_2718(&mut raw.as_ref()).unwrap()),
            );
            let block = BaseBlock {
                header: Header {
                    number,
                    parent_hash,
                    timestamp: cfg.l2_block_timestamp(number),
                    ..Default::default()
                },
                body: BlockBody { transactions, ommers: vec![], withdrawals: None },
            };
            let hash = block.header.hash_slow();
            if number == 0 {
                cfg.genesis.l2 = BlockNumHash { number, hash };
            } else {
                members.push(SingleBatch {
                    parent_hash,
                    epoch_num: epoch.number,
                    epoch_hash: epoch.hash,
                    timestamp: block.header.timestamp,
                    transactions: user,
                });
            }
            provider.blocks.push(L2BlockInfo {
                block_info: BlockInfo {
                    number,
                    hash,
                    parent_hash,
                    timestamp: block.header.timestamp,
                },
                l1_origin: epoch,
                seq_num: 0,
            });
            provider.base_blocks.push(block);
            parent_hash = hash;
        }
        (provider, members)
    }

    #[tokio::test]
    async fn test_overlapping_bls_chunk_releases_only_unsafe_members() {
        let mut cfg = (*dev_config(None)).clone();
        let (mut provider, members) = chain(&mut cfg);
        provider.bls_chunk_verdicts = vec![Some(true)].into();
        let chunk = BlsChunkBatch {
            aggregate: Some(FixedBytes::repeat_byte(0xa1)),
            batches: members.clone(),
        };
        let safe_head = provider.blocks[2];
        let genesis_hash = provider.blocks[0].block_info.hash;
        let prev = TestBatchStreamProvider::new(vec![Ok(Batch::BlsChunk(chunk.clone()))]);
        let mut stream = BatchStream::new(prev, Arc::new(cfg), provider);

        let batch = stream.next_batch(safe_head, &[]).await.unwrap();
        assert_eq!(batch, Batch::Single(members[2].clone()));
        assert_eq!(members[2].parent_hash, safe_head.block_info.hash);
        assert_eq!(stream.chunk_buffer, VecDeque::from(vec![members[3].clone()]));
        // Already-safe members are still authenticated as part of the whole chunk.
        assert_eq!(
            stream.fetcher.bls_chunk_requests,
            vec![(genesis_hash, chunk.block_transactions(), chunk.aggregate)]
        );
    }

    #[tokio::test]
    async fn test_overlapping_bls_chunk_with_altered_safe_member_is_dropped() {
        let mut cfg = (*dev_config(None)).clone();
        let (mut provider, mut members) = chain(&mut cfg);
        provider.bls_chunk_verdicts = vec![Some(true)].into();
        // Reorder the transactions of an already-safe member.
        members[0].transactions.reverse();
        let safe_head = provider.blocks[2];
        let chunk =
            BlsChunkBatch { aggregate: Some(FixedBytes::repeat_byte(0xa1)), batches: members };
        let prev = TestBatchStreamProvider::new(vec![Ok(Batch::BlsChunk(chunk))]);
        let mut stream = BatchStream::new(prev, Arc::new(cfg), provider);

        assert_eq!(
            stream.next_batch(safe_head, &[]).await,
            Err(PipelineError::NotEnoughData.temp())
        );
        assert!(stream.prev.channel_flushed);
        assert!(stream.fetcher.bls_chunk_requests.is_empty());
    }
}
