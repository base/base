//! Built-in [`StageSet`]s.
//!
//! The easiest set to use is [`DefaultStages`], which provides all stages required to run an
//! instance of reth.
//!
//! It is also possible to run parts of reth standalone given the required data is present in
//! the environment, such as [`ExecutionStages`] or [`HashingStages`].
//!
//!
//! # Examples
//!
//! ```no_run
//! # use base_execution_sync_pipeline::Pipeline;
//! # use base_execution_sync_pipeline::{OfflineStages};
//! #
//! # use base_execution_state_types::PruneModes;
//! # use base_execution_evm_blocks::BaseEvmConfig;
//! # use base_execution_state_provider::StaticFileProviderFactory;
//! # use base_execution_state_provider::test_utils::create_test_provider_factory;
//! # use base_execution_state_maintenance::StaticFileProducer;
//! # use reth_config::config::StageConfig;
//! # use std::sync::Arc;
//! # use base_execution_evm_blocks::BaseBeaconConsensus;
//!
//! # fn create(exec: BaseEvmConfig, consensus: BaseBeaconConsensus) {
//!
//! let provider_factory = create_test_provider_factory();
//! let static_file_producer =
//!     StaticFileProducer::new(provider_factory.clone(), PruneModes::default());
//! // Build a pipeline with all offline stages.
//! let pipeline = Pipeline::builder()
//!     .add_stages(OfflineStages::new(exec, Arc::new(consensus), StageConfig::default(), PruneModes::default()))
//!     .build(provider_factory, static_file_producer);
//!
//! # }
//! ```
use std::sync::Arc;

use alloy_primitives::B256;
use base_execution_evm_blocks::BaseBeaconConsensus;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_state_provider::HeaderSyncGapProvider;
use base_execution_state_types::{PruneMode, PruneModes};
use reth_config::config::StageConfig;
use reth_stages_api::Stage;
use tokio::sync::watch;
use {
    base_execution_network_service::BodyDownloader,
    base_execution_network_service::HeaderDownloader,
};

use crate::{
    StageSet, StageSetBuilder,
    stages::{
        AccountHashingStage, BodyStage, ExecutionStage, FinishStage, HeaderStage,
        IndexAccountHistoryStage, IndexStorageHistoryStage, MerkleStage, PruneSenderRecoveryStage,
        PruneStage, SenderRecoveryStage, StorageHashingStage, TransactionLookupStage,
    },
};

/// A set containing all stages to run a fully syncing instance of reth.
///
/// A combination of (in order)
///
/// - [`OnlineStages`]
/// - [`OfflineStages`]
/// - [`FinishStage`]
///
/// This expands to the following series of stages:
/// - [`HeaderStage`]
/// - [`BodyStage`]
/// - [`SenderRecoveryStage`]
/// - [`ExecutionStage`]
/// - [`PruneSenderRecoveryStage`] (execute)
/// - [`MerkleStage`] (unwind)
/// - [`AccountHashingStage`]
/// - [`StorageHashingStage`]
/// - [`MerkleStage`] (execute)
/// - [`TransactionLookupStage`]
/// - [`IndexStorageHistoryStage`]
/// - [`IndexAccountHistoryStage`]
/// - [`PruneStage`] (execute)
/// - [`FinishStage`]
#[derive(Debug)]
pub struct DefaultStages<Provider, H, B>
where
    H: HeaderDownloader,
    B: BodyDownloader,
{
    /// Configuration for the online stages
    online: OnlineStages<Provider, H, B>,
    /// Executor factory needs for execution stage
    evm_config: BaseEvmConfig,
    /// Consensus instance
    consensus: Arc<BaseBeaconConsensus>,
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune configuration for every segment that can be pruned
    prune_modes: PruneModes,
}

impl<Provider, H, B> DefaultStages<Provider, H, B>
where
    H: HeaderDownloader,
    B: BodyDownloader,
{
    /// Create a new set of default stages with default values.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        provider: Provider,
        tip: watch::Receiver<B256>,
        consensus: Arc<BaseBeaconConsensus>,
        header_downloader: H,
        body_downloader: B,
        evm_config: BaseEvmConfig,
        stages_config: StageConfig,
        prune_modes: PruneModes,
    ) -> Self {
        Self {
            online: OnlineStages::new(
                provider,
                tip,
                header_downloader,
                body_downloader,
                stages_config.clone(),
            ),
            evm_config,
            consensus,
            stages_config,
            prune_modes,
        }
    }
}

impl<P, H, B> DefaultStages<P, H, B>
where
    H: HeaderDownloader,
    B: BodyDownloader,
{
    /// Appends the default offline stages and default finish stage to the given builder.
    pub fn add_offline_stages<Provider>(
        default_offline: StageSetBuilder<Provider>,
        evm_config: BaseEvmConfig,
        consensus: Arc<BaseBeaconConsensus>,
        stages_config: StageConfig,
        prune_modes: PruneModes,
    ) -> StageSetBuilder<Provider>
    where
        OfflineStages: StageSet<Provider>,
    {
        StageSetBuilder::default()
            .add_set(default_offline)
            .add_set(OfflineStages::new(evm_config, consensus, stages_config, prune_modes))
            .add_stage(FinishStage)
    }
}

impl<P, H, B, Provider> StageSet<Provider> for DefaultStages<P, H, B>
where
    P: HeaderSyncGapProvider + 'static,
    H: HeaderDownloader + 'static,
    B: BodyDownloader + 'static,
    OnlineStages<P, H, B>: StageSet<Provider>,
    OfflineStages: StageSet<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        Self::add_offline_stages(
            self.online.builder(),
            self.evm_config,
            self.consensus,
            self.stages_config.clone(),
            self.prune_modes,
        )
    }
}

/// A set containing all stages that require network access by default.
///
/// These stages *can* be run without network access if the specified downloaders are
/// themselves offline.
#[derive(Debug)]
pub struct OnlineStages<Provider, H, B>
where
    H: HeaderDownloader,
    B: BodyDownloader,
{
    /// Sync gap provider for the headers stage.
    provider: Provider,
    /// The tip for the headers stage.
    tip: watch::Receiver<B256>,

    /// The block header downloader
    header_downloader: H,
    /// The block body downloader
    body_downloader: B,
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
}

impl<Provider, H, B> OnlineStages<Provider, H, B>
where
    H: HeaderDownloader,
    B: BodyDownloader,
{
    /// Create a new set of online stages with default values.
    pub const fn new(
        provider: Provider,
        tip: watch::Receiver<B256>,
        header_downloader: H,
        body_downloader: B,
        stages_config: StageConfig,
    ) -> Self {
        Self { provider, tip, header_downloader, body_downloader, stages_config }
    }
}

impl<P, H, B> OnlineStages<P, H, B>
where
    P: HeaderSyncGapProvider + 'static,
    H: HeaderDownloader + 'static,
    B: BodyDownloader + 'static,
{
    /// Create a new builder using the given headers stage.
    pub fn builder_with_headers<Provider>(
        headers: HeaderStage<P, H>,
        body_downloader: B,
    ) -> StageSetBuilder<Provider>
    where
        HeaderStage<P, H>: Stage<Provider>,
        BodyStage<B>: Stage<Provider>,
    {
        StageSetBuilder::default().add_stage(headers).add_stage(BodyStage::new(body_downloader))
    }

    /// Create a new builder using the given bodies stage.
    pub fn builder_with_bodies<Provider>(
        bodies: BodyStage<B>,
        provider: P,
        tip: watch::Receiver<B256>,
        header_downloader: H,
        stages_config: StageConfig,
    ) -> StageSetBuilder<Provider>
    where
        BodyStage<B>: Stage<Provider>,
        HeaderStage<P, H>: Stage<Provider>,
    {
        StageSetBuilder::default()
            .add_stage(HeaderStage::new(provider, header_downloader, tip, stages_config.etl))
            .add_stage(bodies)
    }
}

impl<Provider, P, H, B> StageSet<Provider> for OnlineStages<P, H, B>
where
    P: HeaderSyncGapProvider + 'static,
    H: HeaderDownloader + 'static,
    B: BodyDownloader + 'static,
    HeaderStage<P, H>: Stage<Provider>,
    BodyStage<B>: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default()
            .add_stage(HeaderStage::new(
                self.provider,
                self.header_downloader,
                self.tip,
                self.stages_config.etl.clone(),
            ))
            .add_stage(BodyStage::new(self.body_downloader))
    }
}

/// A set containing all stages that do not require network access.
///
/// A combination of (in order)
///
/// - [`ExecutionStages`]
/// - [`PruneSenderRecoveryStage`]
/// - [`HashingStages`]
/// - [`HistoryIndexingStages`]
/// - [`PruneStage`]
#[derive(Debug)]
#[non_exhaustive]
pub struct OfflineStages {
    /// Executor factory needs for execution stage
    evm_config: BaseEvmConfig,
    /// Consensus instance for validating blocks.
    consensus: Arc<BaseBeaconConsensus>,
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune configuration for every segment that can be pruned
    prune_modes: PruneModes,
}

impl OfflineStages {
    /// Create a new set of offline stages with default values.
    pub const fn new(
        evm_config: BaseEvmConfig,
        consensus: Arc<BaseBeaconConsensus>,
        stages_config: StageConfig,
        prune_modes: PruneModes,
    ) -> Self {
        Self { evm_config, consensus, stages_config, prune_modes }
    }
}

impl<Provider> StageSet<Provider> for OfflineStages
where
    ExecutionStages: StageSet<Provider>,
    PruneSenderRecoveryStage: Stage<Provider>,
    HashingStages: StageSet<Provider>,
    HistoryIndexingStages: StageSet<Provider>,
    PruneStage: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        ExecutionStages::new(
            self.evm_config,
            self.consensus,
            self.stages_config.clone(),
            self.prune_modes.sender_recovery,
        )
        .builder()
        // If sender recovery prune mode is set, add the prune sender recovery stage.
        .add_stage_opt(self.prune_modes.sender_recovery.map(|prune_mode| {
            PruneSenderRecoveryStage::new(prune_mode, self.stages_config.prune.commit_threshold)
        }))
        .add_set(HashingStages { stages_config: self.stages_config.clone() })
        .add_set(HistoryIndexingStages {
            stages_config: self.stages_config.clone(),
            prune_modes: self.prune_modes.clone(),
        })
        // Prune stage should be added after all hashing stages, because otherwise it will
        // delete
        .add_stage(PruneStage::new(
            self.prune_modes.clone(),
            self.stages_config.prune.commit_threshold,
        ))
    }
}

/// A set containing all stages that are required to execute pre-existing block data.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecutionStages {
    /// Executor factory that will create executors.
    evm_config: BaseEvmConfig,
    /// Consensus instance for validating blocks.
    consensus: Arc<BaseBeaconConsensus>,
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune mode for sender recovery
    sender_recovery_prune_mode: Option<PruneMode>,
}

impl ExecutionStages {
    /// Create a new set of execution stages with default values.
    pub const fn new(
        executor_provider: BaseEvmConfig,
        consensus: Arc<BaseBeaconConsensus>,
        stages_config: StageConfig,
        sender_recovery_prune_mode: Option<PruneMode>,
    ) -> Self {
        Self { evm_config: executor_provider, consensus, stages_config, sender_recovery_prune_mode }
    }
}

impl<Provider> StageSet<Provider> for ExecutionStages
where
    SenderRecoveryStage: Stage<Provider>,
    ExecutionStage: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default()
            .add_stage(SenderRecoveryStage::new(
                self.stages_config.sender_recovery,
                self.sender_recovery_prune_mode,
            ))
            .add_stage(ExecutionStage::from_config(
                self.evm_config,
                self.consensus,
                self.stages_config.execution,
                self.stages_config.execution_external_clean_threshold(),
            ))
    }
}

/// A set containing all stages that hash account state.
///
/// This includes:
/// - [`MerkleStage`] (unwind)
/// - [`AccountHashingStage`]
/// - [`StorageHashingStage`]
/// - [`MerkleStage`] (execute)
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct HashingStages {
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
}

impl<Provider> StageSet<Provider> for HashingStages
where
    MerkleStage: Stage<Provider>,
    AccountHashingStage: Stage<Provider>,
    StorageHashingStage: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default()
            .add_stage(MerkleStage::default_unwind())
            .add_stage(AccountHashingStage::new(
                self.stages_config.account_hashing,
                self.stages_config.etl.clone(),
            ))
            .add_stage(StorageHashingStage::new(
                self.stages_config.storage_hashing,
                self.stages_config.etl.clone(),
            ))
            .add_stage(MerkleStage::new_execution(
                self.stages_config.merkle.rebuild_threshold,
                self.stages_config.merkle.incremental_threshold,
            ))
    }
}

/// A set containing all stages that do additional indexing for historical state.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct HistoryIndexingStages {
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune configuration for every segment that can be pruned
    prune_modes: PruneModes,
}

impl<Provider> StageSet<Provider> for HistoryIndexingStages
where
    TransactionLookupStage: Stage<Provider>,
    IndexStorageHistoryStage: Stage<Provider>,
    IndexAccountHistoryStage: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default()
            .add_stage(TransactionLookupStage::new(
                self.stages_config.transaction_lookup,
                self.stages_config.etl.clone(),
                self.prune_modes.transaction_lookup,
            ))
            .add_stage(IndexStorageHistoryStage::new(
                self.stages_config.index_storage_history,
                self.stages_config.etl.clone(),
                self.prune_modes.storage_history,
            ))
            .add_stage(IndexAccountHistoryStage::new(
                self.stages_config.index_account_history,
                self.stages_config.etl.clone(),
                self.prune_modes.account_history,
            ))
    }
}
