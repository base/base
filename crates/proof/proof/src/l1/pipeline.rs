//! Contains an oracle-backed pipeline.

use alloc::{boxed::Box, sync::Arc};
use core::fmt::Debug;

use alloy_primitives::Sealable;
use async_trait::async_trait;
use base_consensus_derive::{
    ChainProvider, DataAvailabilityProvider, DerivationPipeline, EthereumDataSource,
    L2ChainProvider, OriginProvider, Pipeline, PipelineBuilder, PipelineErrorKind, PipelineResult,
    PolledAttributesQueueStage, ResetSignal, Signal, SignalReceiver, StatefulAttributesBuilder,
    StepResult,
};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_proof_driver::{DriverPipeline, PipelineCursor};
use base_proof_executor::TrieDBProvider;
use base_proof_preimage::CommsClient;
use base_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
use spin::RwLock;

use crate::{
    FlushableCache, OracleBlobProvider, OracleL1ChainProvider, OracleL2ChainProvider,
    boot::BootInfo,
    sync::{SafeHeadFetcher, new_oracle_pipeline_cursor},
};

/// An oracle-backed derivation pipeline.
pub type ProviderDerivationPipeline<L1, L2, DA> = DerivationPipeline<
    PolledAttributesQueueStage<DA, L1, L2, ProviderAttributesBuilder<L1, L2>>,
    L2,
>;

/// An oracle-backed payload attributes builder for the `AttributesQueue` stage of the derivation
/// pipeline.
pub type ProviderAttributesBuilder<L1, L2> = StatefulAttributesBuilder<L1, L2>;

/// An oracle-backed derivation pipeline.
#[derive(Debug)]
pub struct OraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: L2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// The internal derivation pipeline.
    pub pipeline: ProviderDerivationPipeline<L1, L2, DA>,
    /// The caching oracle.
    pub caching_oracle: Arc<O>,
}

impl<O, L1, L2, DA> OraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: L2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Constructs a new oracle-backed derivation pipeline.
    pub async fn new(
        cfg: Arc<RollupConfig>,
        l1_cfg: Arc<L1ChainConfig>,
        sync_start: Arc<RwLock<PipelineCursor>>,
        caching_oracle: Arc<O>,
        da_provider: DA,
        chain_provider: L1,
        mut l2_chain_provider: L2,
    ) -> PipelineResult<Self> {
        let attributes = StatefulAttributesBuilder::new(
            Arc::clone(&cfg),
            l1_cfg,
            l2_chain_provider.clone(),
            chain_provider.clone(),
        );

        let cfg_for_reset = Arc::clone(&cfg);

        let mut pipeline = PipelineBuilder::new()
            .rollup_config(cfg)
            .dap_source(da_provider)
            .l2_chain_provider(l2_chain_provider.clone())
            .chain_provider(chain_provider)
            .builder(attributes)
            .origin(sync_start.read().origin())
            .build_polled();

        // Reset the pipeline to populate the initial system configuration in L1 Traversal.
        let l2_safe_head = *sync_start.read().l2_safe_head();
        pipeline
            .signal(
                ResetSignal {
                    l2_safe_head,
                    l1_origin: sync_start.read().origin(),
                    system_config: l2_chain_provider
                        .system_config_by_number(l2_safe_head.block_info.number, cfg_for_reset)
                        .await
                        .ok(),
                }
                .signal(),
            )
            .await?;

        Ok(Self { pipeline, caching_oracle })
    }
}

impl<O>
    OraclePipeline<
        O,
        OracleL1ChainProvider<O>,
        OracleL2ChainProvider<O>,
        EthereumDataSource<OracleL1ChainProvider<O>, OracleBlobProvider<O>>,
    >
where
    O: CommsClient + FlushableCache + Send + Sync + Debug + Clone,
{
    /// Constructs a new oracle-backed derivation pipeline from boot info and an oracle.
    ///
    /// This factory method sets up all the required providers and cursors from the boot info,
    /// making it the primary entry point for pipeline initialization.
    pub async fn from_boot_info(
        oracle: Arc<O>,
        boot_info: &BootInfo,
    ) -> Result<(Self, Arc<RwLock<PipelineCursor>>), PipelineErrorKind> {
        let cfg = Arc::new(boot_info.rollup_config.clone());
        let l1_cfg = Arc::new(boot_info.l1_config.clone());

        let l2_head_hash =
            SafeHeadFetcher::fetch(oracle.as_ref(), boot_info.agreed_l2_output_root).await?;

        let mut l1_provider = OracleL1ChainProvider::new(boot_info.l1_head, Arc::clone(&oracle));
        let mut l2_provider =
            OracleL2ChainProvider::new(l2_head_hash, Arc::clone(&cfg), Arc::clone(&oracle));
        let blob_provider = OracleBlobProvider::new(Arc::clone(&oracle));
        let da_provider =
            EthereumDataSource::new_from_parts(l1_provider.clone(), blob_provider, &cfg);

        let safe_header = l2_provider.header_by_hash(l2_head_hash)?.seal_slow();

        let cursor =
            new_oracle_pipeline_cursor(&cfg, safe_header, &mut l1_provider, &mut l2_provider)
                .await?;
        l2_provider.set_cursor(Arc::clone(&cursor));

        let pipeline = Self::new(
            cfg,
            l1_cfg,
            Arc::clone(&cursor),
            Arc::clone(&oracle),
            da_provider,
            l1_provider,
            l2_provider,
        )
        .await?;

        Ok((pipeline, cursor))
    }
}

impl<O, L1, L2, DA> DriverPipeline<ProviderDerivationPipeline<L1, L2, DA>>
    for OraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: L2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Flushes the cache on re-org.
    fn flush(&mut self) {
        self.caching_oracle.flush();
    }
}

#[async_trait]
impl<O, L1, L2, DA> SignalReceiver for OraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: L2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Receives a signal from the driver.
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        self.pipeline.signal(signal).await
    }
}

impl<O, L1, L2, DA> OriginProvider for OraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: L2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Returns the optional L1 [`BlockInfo`] origin.
    fn origin(&self) -> Option<BlockInfo> {
        self.pipeline.origin()
    }
}

impl<O, L1, L2, DA> Iterator for OraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: L2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    type Item = OpAttributesWithParent;

    fn next(&mut self) -> Option<Self::Item> {
        self.pipeline.next()
    }
}

#[async_trait]
impl<O, L1, L2, DA> Pipeline for OraclePipeline<O, L1, L2, DA>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: L2ChainProvider + Send + Sync + Debug + Clone,
    DA: DataAvailabilityProvider + Send + Sync + Debug + Clone,
{
    /// Peeks at the next [`OpAttributesWithParent`] from the pipeline.
    fn peek(&self) -> Option<&OpAttributesWithParent> {
        self.pipeline.peek()
    }

    /// Attempts to progress the pipeline.
    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult {
        self.pipeline.step(cursor).await
    }

    /// Returns the rollup config.
    fn rollup_config(&self) -> &RollupConfig {
        self.pipeline.rollup_config()
    }

    /// Returns the [`SystemConfig`] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
    ) -> Result<SystemConfig, PipelineErrorKind> {
        self.pipeline.system_config_by_number(number).await
    }
}
