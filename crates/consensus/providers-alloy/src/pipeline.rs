//! Contains an online derivation pipeline.

use core::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use base_consensus_derive::{
    DerivationPipeline, EthereumDataSource, IndexedAttributesQueueStage, L2ChainProvider,
    OriginProvider, Pipeline, PipelineBuilder, PipelineErrorKind, PipelineResult,
    PolledAttributesQueueStage, ResetSignal, Signal, SignalReceiver, StatefulAttributesBuilder,
    StepResult,
};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};

use crate::{
    AlloyChainProvider, AlloyL2ChainProvider, ConfDepthProvider, L1HeadNumber, OnlineBeaconClient,
    OnlineBlobProvider,
};

/// An online polled derivation pipeline (verifier mode — enforces confirmation depth).
type OnlinePolledDerivationPipeline = DerivationPipeline<
    PolledAttributesQueueStage<
        OnlinePolledDataProvider,
        ConfDepthProvider,
        AlloyL2ChainProvider,
        OnlinePolledAttributesBuilder,
    >,
    AlloyL2ChainProvider,
>;

/// An online managed derivation pipeline (sequencer mode — no confirmation depth).
type OnlineManagedDerivationPipeline = DerivationPipeline<
    IndexedAttributesQueueStage<
        OnlineDataProvider,
        AlloyChainProvider,
        AlloyL2ChainProvider,
        OnlineAttributesBuilder,
    >,
    AlloyL2ChainProvider,
>;

/// An RPC-backed Ethereum data source for the polled (verifier) pipeline.
type OnlinePolledDataProvider =
    EthereumDataSource<ConfDepthProvider, OnlineBlobProvider<OnlineBeaconClient>>;

/// An RPC-backed payload attributes builder for the polled (verifier) pipeline.
type OnlinePolledAttributesBuilder =
    StatefulAttributesBuilder<ConfDepthProvider, AlloyL2ChainProvider>;

/// An RPC-backed Ethereum data source for the managed (sequencer) pipeline.
type OnlineDataProvider =
    EthereumDataSource<AlloyChainProvider, OnlineBlobProvider<OnlineBeaconClient>>;

/// An RPC-backed payload attributes builder for the managed (sequencer) pipeline.
type OnlineAttributesBuilder = StatefulAttributesBuilder<AlloyChainProvider, AlloyL2ChainProvider>;

/// An online derivation pipeline.
#[derive(Debug)]
pub enum OnlinePipeline {
    /// An online derivation pipeline that uses a polled traversal stage.
    Polled(Box<OnlinePolledDerivationPipeline>),
    /// An online derivation pipeline that uses a managed traversal stage.
    Managed(Box<OnlineManagedDerivationPipeline>),
}

impl OnlinePipeline {
    /// Constructs a new polled derivation pipeline that is initialized.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        cfg: Arc<RollupConfig>,
        l1_cfg: Arc<L1ChainConfig>,
        l2_safe_head: L2BlockInfo,
        l1_origin: BlockInfo,
        blob_provider: OnlineBlobProvider<OnlineBeaconClient>,
        chain_provider: AlloyChainProvider,
        mut l2_chain_provider: AlloyL2ChainProvider,
        l1_head_number: L1HeadNumber,
        verifier_l1_confs: u64,
    ) -> PipelineResult<Self> {
        let mut pipeline = Self::new_polled(
            Arc::clone(&cfg),
            Arc::clone(&l1_cfg),
            blob_provider,
            chain_provider,
            l2_chain_provider.clone(),
            l1_head_number,
            verifier_l1_confs,
        );

        // Reset the pipeline to populate the initial L1/L2 cursor and system configuration in L1
        // Traversal.
        pipeline
            .signal(
                ResetSignal {
                    l2_safe_head,
                    l1_origin,
                    system_config: l2_chain_provider
                        .system_config_by_number(l2_safe_head.block_info.number, Arc::clone(&cfg))
                        .await
                        .ok(),
                }
                .signal(),
            )
            .await?;

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
        l1_cfg: Arc<L1ChainConfig>,
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

        Self::Polled(Box::new(pipeline))
    }

    /// Constructs a new indexed derivation pipeline that is uninitialized.
    ///
    /// Uses online providers as specified by the arguments.
    ///
    /// Before using the returned pipeline, a [`ResetSignal`] must be sent to
    /// instantiate the pipeline state. [`Self::new`] is a convenience method that
    /// constructs a new online pipeline and sends the reset signal.
    pub fn new_indexed(
        cfg: Arc<RollupConfig>,
        l1_cfg: Arc<L1ChainConfig>,
        blob_provider: OnlineBlobProvider<OnlineBeaconClient>,
        chain_provider: AlloyChainProvider,
        l2_chain_provider: AlloyL2ChainProvider,
    ) -> Self {
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
            .build_indexed();

        Self::Managed(Box::new(pipeline))
    }
}

#[async_trait]
impl SignalReceiver for OnlinePipeline {
    /// Receives a signal from the driver.
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        match self {
            Self::Polled(pipeline) => pipeline.signal(signal).await,
            Self::Managed(pipeline) => pipeline.signal(signal).await,
        }
    }
}

impl OriginProvider for OnlinePipeline {
    /// Returns the optional L1 [`BlockInfo`] origin.
    fn origin(&self) -> Option<BlockInfo> {
        match self {
            Self::Polled(pipeline) => pipeline.origin(),
            Self::Managed(pipeline) => pipeline.origin(),
        }
    }
}

impl Iterator for OnlinePipeline {
    type Item = OpAttributesWithParent;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Polled(pipeline) => pipeline.next(),
            Self::Managed(pipeline) => pipeline.next(),
        }
    }
}

#[async_trait]
impl Pipeline for OnlinePipeline {
    /// Peeks at the next [`OpAttributesWithParent`] from the pipeline.
    fn peek(&self) -> Option<&OpAttributesWithParent> {
        match self {
            Self::Polled(pipeline) => pipeline.peek(),
            Self::Managed(pipeline) => pipeline.peek(),
        }
    }

    /// Attempts to progress the pipeline.
    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult {
        match self {
            Self::Polled(pipeline) => pipeline.step(cursor).await,
            Self::Managed(pipeline) => pipeline.step(cursor).await,
        }
    }

    /// Returns the rollup config.
    fn rollup_config(&self) -> &RollupConfig {
        match self {
            Self::Polled(pipeline) => pipeline.rollup_config(),
            Self::Managed(pipeline) => pipeline.rollup_config(),
        }
    }

    /// Returns the [`SystemConfig`] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
    ) -> Result<SystemConfig, PipelineErrorKind> {
        match self {
            Self::Polled(pipeline) => pipeline.system_config_by_number(number).await,
            Self::Managed(pipeline) => pipeline.system_config_by_number(number).await,
        }
    }
}
