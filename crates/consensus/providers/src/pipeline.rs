//! Contains an online derivation pipeline.

use core::fmt::Debug;
use std::sync::Arc;

use alloy_genesis::ChainConfig;
use async_trait::async_trait;
use base_consensus_derive::{
    DerivationPipeline, EthereumDataSource, OriginProvider, Pipeline, PipelineBuilder,
    PipelineErrorKind, PipelineResult, PolledAttributesQueueStage, ResetSignal, Signal,
    SignalReceiver, StatefulAttributesBuilder, StepResult,
};
use base_consensus_genesis::{RollupConfig, SystemConfig};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

use crate::{
    AlloyChainProvider, AlloyL2ChainProvider, ConfDepthProvider, L1HeadNumber, OnlineBeaconClient,
    OnlineBlobProvider,
};

/// An online polled derivation pipeline.
type OnlinePolledDerivationPipeline = DerivationPipeline<
    PolledAttributesQueueStage<
        OnlineDataProvider,
        ConfDepthProvider,
        AlloyL2ChainProvider,
        OnlineAttributesBuilder,
    >,
    AlloyL2ChainProvider,
>;

/// An RPC-backed Ethereum data source.
type OnlineDataProvider =
    EthereumDataSource<ConfDepthProvider, OnlineBlobProvider<OnlineBeaconClient>>;

/// An RPC-backed payload attributes builder for the `AttributesQueue` stage of the derivation
/// pipeline.
type OnlineAttributesBuilder = StatefulAttributesBuilder<ConfDepthProvider, AlloyL2ChainProvider>;

/// An online derivation pipeline.
#[derive(Debug)]
pub struct OnlinePipeline {
    /// The inner polled derivation pipeline.
    inner: OnlinePolledDerivationPipeline,
}

impl OnlinePipeline {
    /// Constructs a new polled derivation pipeline that is initialized.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        cfg: Arc<RollupConfig>,
        l1_cfg: Arc<ChainConfig>,
        l2_safe_head: L2BlockInfo,
        blob_provider: OnlineBlobProvider<OnlineBeaconClient>,
        chain_provider: AlloyChainProvider,
        l2_chain_provider: AlloyL2ChainProvider,
        l1_head_number: L1HeadNumber,
        verifier_l1_confs: u64,
    ) -> PipelineResult<Self> {
        let mut pipeline = Self::new_polled(
            Arc::clone(&cfg),
            Arc::clone(&l1_cfg),
            blob_provider,
            chain_provider,
            l2_chain_provider,
            l1_head_number,
            verifier_l1_confs,
        );

        // Reset the pipeline to populate the initial L1/L2 cursor and system configuration in L1
        // Traversal.
        pipeline.signal(ResetSignal { l2_safe_head }.signal()).await?;

        Ok(pipeline)
    }

    /// Constructs a new polled derivation pipeline that is uninitialized.
    ///
    /// Uses online providers as specified by the arguments.
    ///
    /// Before using the returned pipeline, a [`ResetSignal`] must be sent to
    /// instantiate the pipeline state. [`Self::new`] is a convenience method that
    /// constructs a new online pipeline and sends the reset signal.
    #[allow(clippy::too_many_arguments)]
    pub fn new_polled(
        cfg: Arc<RollupConfig>,
        l1_cfg: Arc<ChainConfig>,
        blob_provider: OnlineBlobProvider<OnlineBeaconClient>,
        chain_provider: AlloyChainProvider,
        l2_chain_provider: AlloyL2ChainProvider,
        l1_head_number: L1HeadNumber,
        verifier_l1_confs: u64,
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
            .build_polled();

        Self { inner: pipeline }
    }
}

#[async_trait]
impl SignalReceiver for OnlinePipeline {
    /// Receives a signal from the driver.
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        self.inner.signal(signal).await
    }
}

impl OriginProvider for OnlinePipeline {
    /// Returns the optional L1 [`BlockInfo`] origin.
    fn origin(&self) -> Option<BlockInfo> {
        self.inner.origin()
    }
}

impl Iterator for OnlinePipeline {
    type Item = AttributesWithParent;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[async_trait]
impl Pipeline for OnlinePipeline {
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

    /// Returns the [`SystemConfig`] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
    ) -> Result<SystemConfig, PipelineErrorKind> {
        self.inner.system_config_by_number(number).await
    }
}
