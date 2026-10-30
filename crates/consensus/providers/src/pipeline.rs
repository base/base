//! Contains an online derivation pipeline.

use core::fmt::Debug;
use std::sync::Arc;

use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_consensus_derive::{
    BlobProvider, ChainProvider, DerivationPipeline, EthereumDataSource, OriginProvider, Pipeline,
    PipelineBuilder, PipelineErrorKind, PipelineResult, PolledAttributesQueueStage, Signal,
    SignalReceiver, StatefulAttributesBuilder, StepResult,
};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

use crate::{
    AlloyChainProvider, AlloyChainProviderError, AlloyL2ChainProvider, ConfDepthProvider,
    L1HeadNumber, OnlineBeaconClient, OnlineBlobProvider,
};

/// An L1 chain provider an [`OnlinePipeline`] can read from.
pub trait OnlineChainProvider:
    ChainProvider<Error: From<AlloyChainProviderError> + Send> + Clone + Debug + Send + Sync + 'static
{
}

impl<T> OnlineChainProvider for T where
    T: ChainProvider<Error: From<AlloyChainProviderError> + Send>
        + Clone
        + Debug
        + Send
        + Sync
        + 'static
{
}

/// A blob provider an [`OnlinePipeline`] can read from.
pub trait OnlineBlobSource: BlobProvider + Clone + Debug + Send + Sync + 'static {}

impl<T> OnlineBlobSource for T where T: BlobProvider + Clone + Debug + Send + Sync + 'static {}

/// An online polled derivation pipeline.
type OnlinePolledDerivationPipeline<C, B> = DerivationPipeline<
    PolledAttributesQueueStage<
        EthereumDataSource<ConfDepthProvider<C>, B>,
        ConfDepthProvider<C>,
        AlloyL2ChainProvider,
        StatefulAttributesBuilder<ConfDepthProvider<C>, AlloyL2ChainProvider>,
    >,
    AlloyL2ChainProvider,
>;

/// An online derivation pipeline reading L1 chain data from `C` and blobs from `B`.
#[derive(Debug)]
pub struct OnlinePipeline<C = AlloyChainProvider, B = OnlineBlobProvider<OnlineBeaconClient>>
where
    C: OnlineChainProvider,
    B: OnlineBlobSource,
{
    /// The inner polled derivation pipeline.
    inner: OnlinePolledDerivationPipeline<C, B>,
}

impl<C, B> OnlinePipeline<C, B>
where
    C: OnlineChainProvider,
    B: OnlineBlobSource,
{
    /// Constructs a new uninitialized polled derivation pipeline with an optional batcher address
    /// override used only to filter L1 data-availability transactions.
    #[allow(clippy::too_many_arguments)]
    pub fn new_polled_with_da_batcher_sender_override(
        cfg: Arc<RollupConfig>,
        l1_cfg: Arc<ChainConfig>,
        blob_provider: B,
        chain_provider: C,
        l2_chain_provider: AlloyL2ChainProvider,
        l1_head_number: L1HeadNumber,
        verifier_l1_confs: u64,
        da_batcher_sender_override: Option<Address>,
    ) -> Self {
        let chain_provider =
            ConfDepthProvider::new(chain_provider, l1_head_number, verifier_l1_confs);
        let attributes = StatefulAttributesBuilder::new(
            Arc::clone(&cfg),
            l1_cfg,
            l2_chain_provider.clone(),
            chain_provider.clone(),
        );
        let dap = EthereumDataSource::new_from_parts(chain_provider.clone(), blob_provider, &cfg);

        let pipeline = PipelineBuilder::new()
            .rollup_config(cfg)
            .dap_source(dap)
            .l2_chain_provider(l2_chain_provider)
            .chain_provider(chain_provider)
            .builder(attributes)
            .origin(BlockInfo::default())
            .da_batcher_sender_override(da_batcher_sender_override)
            .build_polled();

        Self { inner: pipeline }
    }
}

#[async_trait]
impl<C, B> SignalReceiver for OnlinePipeline<C, B>
where
    C: OnlineChainProvider,
    B: OnlineBlobSource,
{
    /// Receives a signal from the driver.
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        self.inner.signal(signal).await
    }
}

impl<C, B> OriginProvider for OnlinePipeline<C, B>
where
    C: OnlineChainProvider,
    B: OnlineBlobSource,
{
    /// Returns the optional L1 [`BlockInfo`] origin.
    fn origin(&self) -> Option<BlockInfo> {
        self.inner.origin()
    }
}

impl<C, B> Iterator for OnlinePipeline<C, B>
where
    C: OnlineChainProvider,
    B: OnlineBlobSource,
{
    type Item = AttributesWithParent;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[async_trait]
impl<C, B> Pipeline for OnlinePipeline<C, B>
where
    C: OnlineChainProvider,
    B: OnlineBlobSource,
{
    /// Peeks at the next [`AttributesWithParent`] from the pipeline.
    fn peek(&self) -> Option<&AttributesWithParent> {
        self.inner.peek()
    }

    /// Attempts to progress the pipeline.
    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult {
        self.inner.step(cursor).await
    }

    /// Returns the rollup config.
    fn rollup_config(&self) -> &RollupConfig {
        self.inner.rollup_config()
    }

    /// Returns the [`SystemConfig`] for the L2 block with the given hash.
    async fn system_config_by_l2_hash(
        &mut self,
        hash: B256,
    ) -> Result<SystemConfig, PipelineErrorKind> {
        self.inner.system_config_by_l2_hash(hash).await
    }
}
