//! Concrete stages for configuring storage and assembling a Base node.
//!
//! Configuration is loaded before creating the provider factory. The provider stage
//! initializes storage before assembling the node's runtime components.

use std::{num::NonZeroUsize, sync::Arc, thread::available_parallelism, time::Duration};

use alloy_eips::eip2124::Head;
use alloy_primitives::{B256, BlockNumber};
use base_common_io as fs;
use base_common_observability_tracing::{
    throttle,
    tracing::{debug, error, info, warn},
};
use base_common_runtime::TaskExecutor;
use base_common_types_payload::ConsensusEngineEvent;
use base_execution_evm_blocks::{BaseBeaconConsensus, BaseEvmConfig};
use base_execution_network_wire::HeadersClient;
use base_execution_state_database::{DatabaseMetrics, models::PartialStateTrieUnwindMarker};
use base_execution_state_operations::{
    PruneMode, PruneModes, PrunerBuilder, StaticFileProducer, StaticFileSegment,
    blocks_per_file_for_prune_distance,
    init::{InitStorageError, init_genesis_with_settings, init_genesis_with_settings_and_validate},
};
use base_execution_state_provider::{
    BalConfig, BalStoreHandle, BlockHashReader, DBProvider, DatabaseProviderFactory,
    DatabaseProviderROFactory, InMemoryBalStore, MetadataProvider, OverlayManager, ProviderError,
    ProviderFactory, ProviderResult, RocksDBProviderFactory, StageCheckpointReader,
    StaticFileProviderBuilder, StaticFileProviderFactory, StorageSettingsCache,
    providers::{BlockchainProvider, RocksDBProvider},
};
use base_execution_state_types::{EtlConfig, PruneConfig};
use base_execution_sync::{
    DefaultStages, MerkleStage, MetricEvent, NoopBodiesDownloader, NoopHeaderDownloader,
    PipelineBuilder, PipelineTarget, StageId, StageSet,
};
use base_node_config::{ChainPath, DataDirPath, NodeConfig, PruneConfigKind, version_metadata};
use eyre::Context;
use futures::{Stream, StreamExt, future::Either, stream};
use rayon::ThreadPoolBuilder;
use tokio::sync::{
    mpsc::{UnboundedSender, unbounded_channel},
    oneshot, watch,
};

use crate::{
    BaseNode, BaseNodeContext, BuilderContext, ChainSpecInfo, ConsensusLayerHealthEvents,
    EthStatsService, ExExLauncher, Hooks, MetricServer, MetricServerConfig, NodeEvent,
    StorageSettingsInfo, VersionInfo, install_prometheus_recorder,
};

/// Process resources shared by the node launch stages.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    /// The task executor for the node.
    pub task_executor: TaskExecutor,
    /// The data directory for the node.
    pub data_dir: ChainPath<DataDirPath>,
}

impl LaunchContext {
    /// Create a new instance of the default node launcher.
    pub const fn new(task_executor: TaskExecutor, data_dir: ChainPath<DataDirPath>) -> Self {
        Self { task_executor, data_dir }
    }

    /// Loads the reth config with the configured `data_dir` and overrides settings according to the
    /// `config`.
    ///
    /// Returns the configuration stage with the loaded `reth.toml` settings.
    pub fn with_loaded_toml_config(self, config: NodeConfig) -> eyre::Result<ConfiguredLaunch> {
        let toml_config = self.load_toml_config(&config)?;
        Ok(ConfiguredLaunch { context: self, configs: WithConfigs { config, toml_config } })
    }

    /// Loads the reth config with the configured `data_dir` and overrides settings according to the
    /// `config`.
    pub fn load_toml_config(
        &self,
        config: &NodeConfig,
    ) -> eyre::Result<base_node_config::NodeFileConfig> {
        let config_path = config.config.clone().unwrap_or_else(|| self.data_dir.config());

        let mut toml_config = base_node_config::NodeFileConfig::from_path(&config_path)
            .wrap_err_with(|| format!("Could not load config file {config_path:?}"))?;

        Self::save_pruning_config(&mut toml_config, config, &config_path)?;

        info!(target: "reth::cli", path = ?config_path, "Configuration loaded");

        // Update the config with the command line arguments. Only override when the CLI flag is
        // set, so the TOML value is preserved when the flag is not passed.
        toml_config.peers.trusted_nodes_only |= config.network.trusted_only;

        // Merge static file CLI arguments with config file, giving priority to CLI
        toml_config.static_files =
            config.static_files.merge_with_config(toml_config.static_files, config.pruning.minimal);

        Ok(toml_config)
    }

    /// Save prune config to the toml file if node is a full node or has custom pruning CLI
    /// arguments. Also migrates deprecated prune config values to new defaults.
    fn save_pruning_config(
        reth_config: &mut base_node_config::NodeFileConfig,
        config: &NodeConfig,
        config_path: impl AsRef<std::path::Path>,
    ) -> eyre::Result<()> {
        let mut should_save = reth_config.prune.segments.migrate();

        if let Some(prune_config) = config.prune_config() {
            if reth_config.prune != prune_config {
                reth_config.set_prune_config(prune_config);
                should_save = true;
            }
        } else if !reth_config.prune.is_default() {
            info!(target: "reth::cli", "Pruning configuration is present in the config file, but no CLI arguments are provided. Using config from file.");
        }

        if should_save {
            info!(target: "reth::cli", "Saving prune config to toml file");
            reth_config.save(config_path.as_ref())?;
        }

        Ok(())
    }

    /// Configure global settings this includes:
    ///
    /// - Raising the file descriptor limit
    /// - Configuring the global rayon thread pool for implicit `par_iter` usage
    pub fn configure_globals(&self) {
        // Raise the fd limit of the process.
        // Does not do anything on windows.
        match fdlimit::raise_fd_limit() {
            Ok(fdlimit::Outcome::LimitRaised { from, to }) => {
                debug!(from, to, "Raised file descriptor limit");
            }
            Ok(fdlimit::Outcome::Unsupported) => {}
            Err(err) => warn!(%err, "Failed to raise file descriptor limit"),
        }

        // Configure the implicit global rayon pool for `par_iter` usage.
        let num_threads = available_parallelism().map_or(1, NonZeroUsize::get);
        if let Err(err) = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("rayon-{i:02}"))
            .build_global()
        {
            warn!(%err, "Failed to build global thread pool")
        }
    }
}

/// Loaded configuration and process resources for opening node storage.
#[derive(Debug, Clone)]
pub struct ConfiguredLaunch {
    /// Process resources used throughout startup.
    pub context: LaunchContext,
    /// CLI and file configuration.
    pub configs: WithConfigs,
}

impl ConfiguredLaunch {
    /// Adds the configured trusted peers to the TOML settings.
    pub fn with_resolved_peers(mut self) -> Self {
        if !self.configs.config.network.trusted_peers.is_empty() {
            info!(target: "reth::cli", "Adding trusted nodes");

            self.configs
                .toml_config
                .peers
                .trusted_nodes
                .extend(self.configs.config.network.trusted_peers.clone());
        }
        self
    }

    /// Adjust certain settings in the config to make sure they are set correctly
    ///
    /// This includes:
    /// - Making sure the ETL dir is set to the datadir
    /// - RPC settings are adjusted to the correct port
    pub fn with_adjusted_configs(self) -> Self {
        self.ensure_etl_datadir().with_adjusted_instance_ports()
    }

    /// Make sure ETL doesn't default to /tmp/, but to whatever datadir is set to
    pub fn ensure_etl_datadir(mut self) -> Self {
        if self.configs.toml_config.stages.etl.dir.is_none() {
            let etl_path = EtlConfig::from_datadir(self.context.data_dir.data_dir());
            if etl_path.exists() {
                // Remove etl-path files on launch
                if let Err(err) = fs::Files::remove_dir_all(&etl_path) {
                    warn!(target: "reth::cli", ?etl_path, %err, "Failed to remove ETL path on launch");
                }
            }
            self.configs.toml_config.stages.etl.dir = Some(etl_path);
        }

        self
    }

    /// Change rpc port numbers based on the instance number.
    pub fn with_adjusted_instance_ports(mut self) -> Self {
        self.configs.config.adjust_instance_ports();
        self
    }

    /// Returns the configured [`PruneConfig`]
    ///
    /// Any configuration set in CLI will take precedence over those set in toml
    pub fn prune_config(&self) -> PruneConfig {
        let Some(mut node_prune_config) = self.configs.config.prune_config() else {
            // No CLI config is set, use the toml config.
            return self.configs.toml_config.prune.clone();
        };

        // Otherwise, use the CLI configuration and merge with toml config.
        node_prune_config.merge(self.configs.toml_config.prune.clone());
        node_prune_config
    }

    /// Returns the configured [`PruneModes`], returning the default if no config was available.
    pub fn prune_modes(&self) -> PruneModes {
        self.prune_config().segments
    }

    /// Returns an initialized [`PrunerBuilder`] based on the configured [`PruneConfig`]
    pub fn pruner_builder(&self) -> PrunerBuilder {
        PrunerBuilder::new(self.prune_config())
    }

    /// Returns the [`ProviderFactory`] after checking consistency
    /// between the database and static files. **It may execute a pipeline unwind if it fails this
    /// check.**
    pub async fn create_provider_factory(
        &self,
        database: &base_execution_state_database::DatabaseEnv,
        overlay_manager: OverlayManager,
        disabled_stages: &[StageId],
    ) -> eyre::Result<ProviderFactory> {
        // Validate static files configuration
        let static_files_config = &self.configs.toml_config.static_files;
        static_files_config.validate()?;

        let prune_config = self.prune_config();

        let mut blocks_per_file = static_files_config.as_blocks_per_file_map();
        // Receipts in static files are pruned by deleting whole files, so with the default file
        // size a distance-based prune target is only reached every 500k blocks. Unless a file size
        // is explicitly configured, derive one from the prune distance so retention tracks the
        // configured distance.
        if blocks_per_file.get(StaticFileSegment::Receipts).is_none()
            && let Some(PruneMode::Distance(distance)) = prune_config.segments.receipts
        {
            blocks_per_file
                .insert(StaticFileSegment::Receipts, blocks_per_file_for_prune_distance(distance));
        }

        // Apply per-segment blocks_per_file configuration
        let static_file_provider =
            StaticFileProviderBuilder::read_write(self.context.data_dir.static_files())
                .with_metrics()
                .with_blocks_per_file_for_segments(&blocks_per_file)
                .with_genesis_block_number(
                    self.configs.config.chain.genesis().number.unwrap_or_default(),
                )
                .build()?;

        let rocksdb_provider = RocksDBProvider::builder(self.context.data_dir.rocksdb())
            .with_default_tables()
            .with_metrics()
            .with_statistics()
            .build()?;

        let balstore_cache_size = self
            .configs
            .config
            .db
            .balstore_cache_size
            .unwrap_or(BalConfig::DEFAULT_IN_MEMORY_RETENTION_DISTANCE);
        let bal_store = BalStoreHandle::new(InMemoryBalStore::new(
            BalConfig::with_in_memory_retention_distance(balstore_cache_size),
        ));
        let factory = ProviderFactory::new(
            database.clone(),
            Arc::clone(&self.configs.config.chain),
            static_file_provider,
            rocksdb_provider,
            self.context.task_executor.clone(),
        )?
        .with_prune_modes(prune_config.segments)
        .with_minimum_pruning_distance(prune_config.minimum_pruning_distance)
        .with_overlay_manager(overlay_manager)
        .with_bal_store(bal_store);

        // Check consistency between the database and static files, returning
        // the unwind targets for each storage layer if inconsistencies are
        // found.
        let (rocksdb_unwind, static_file_unwind) = factory.check_consistency()?;
        let provider_ro = factory.database_provider_ro()?;
        // Finish is committed before Merkle during unwind, so this marker is authoritative when
        // resuming an interrupted partial trie unwind.
        let (partial_trie_unwind, has_persisted_partial_trie_unwind) =
            get_partial_trie_unwind_marker(&provider_ro)?;
        drop(provider_ro);
        let persist_partial_trie_unwind =
            !has_persisted_partial_trie_unwind && partial_trie_unwind.is_some();
        let partial_trie_unwind_target =
            partial_trie_unwind.map(|marker| marker.partial_state_trie);
        // Recover the partial state trie first. Its unwind enables
        // `walk_all_changed_branch_children`, which is more expensive than a normal unwind, so
        // it only runs to the partial trie target. A lower storage-layer target is then unwound
        // normally.
        let storage_unwind = [rocksdb_unwind, static_file_unwind].into_iter().flatten().min();
        let storage_unwind = storage_unwind.filter(|unwind_block| {
            partial_trie_unwind_target.is_none_or(|partial_trie| *unwind_block < partial_trie)
        });

        if partial_trie_unwind_target.is_some() || storage_unwind.is_some() {
            let build_unwind_pipeline = |walk_all_changed_branch_children| {
                let (_tip_tx, tip_rx) = watch::channel(B256::ZERO);
                let mut stages = DefaultStages::new(
                    factory.clone(),
                    tip_rx,
                    Arc::new(BaseBeaconConsensus::noop()),
                    NoopHeaderDownloader::default(),
                    NoopBodiesDownloader::default(),
                    BaseEvmConfig::default(),
                    self.configs.toml_config.stages.clone(),
                    self.prune_modes(),
                )
                .builder()
                .disable_all(disabled_stages);

                if walk_all_changed_branch_children {
                    // Partial trie recovery is not complete until Merkle has unwound.
                    stages =
                        stages.set(MerkleStage::new_unwind(true)).enable(StageId::MerkleUnwind);
                }

                PipelineBuilder::default().add_stages(stages).build(
                    factory.clone(),
                    StaticFileProducer::new(factory.clone(), self.prune_modes()),
                )
            };
            let mut unwinds = Vec::with_capacity(2);

            if let Some(unwind_block) = partial_trie_unwind_target {
                unwinds.push((
                    PipelineTarget::Unwind(unwind_block),
                    "partial state trie".to_owned(),
                    build_unwind_pipeline(true),
                    true,
                ));
            }

            if let Some(unwind_block) = storage_unwind {
                // Highly unlikely to happen, and given its destructive nature, it's better to
                // panic instead. Unwinding to 0 would leave MDBX with a huge free list size.
                let inconsistency_source = match (rocksdb_unwind, static_file_unwind) {
                    (Some(_), Some(_)) => "RocksDB and static file",
                    (Some(_), None) => "RocksDB",
                    (None, Some(_)) => "static file",
                    (None, None) => unreachable!(),
                };
                assert_ne!(
                    unwind_block, 0,
                    "A {inconsistency_source} inconsistency was found that would trigger an unwind to block 0"
                );
                unwinds.push((
                    PipelineTarget::Unwind(unwind_block),
                    inconsistency_source.to_owned(),
                    build_unwind_pipeline(false),
                    false,
                ));
            }

            if persist_partial_trie_unwind {
                // The marker must be durable before any unwind stage can commit.
                let provider_rw = factory.database_provider_rw()?;
                write_partial_trie_unwind_marker(
                    &provider_rw,
                    partial_trie_unwind.expect("partial trie unwind marker must exist"),
                )?;
                provider_rw.commit()?;
            }

            let (tx, rx) = oneshot::channel();
            let factory = factory.clone();

            // Pipeline should be run as blocking and panic if it fails.
            self.context.task_executor.spawn_critical_blocking_task("pipeline task", async move {
                let result: Result<(), base_execution_sync::PipelineError> = async {
                    for (unwind_target, inconsistency_source, pipeline, clear_partial_trie_unwind) in
                        unwinds
                    {
                        info!(target: "reth::cli", %unwind_target, %inconsistency_source, "Executing unwind after consistency check.");
                        let (_, result) = pipeline.run_as_fut(Some(unwind_target)).await;
                        result.inspect_err(|err| {
                            error!(target: "reth::cli", %unwind_target, %inconsistency_source, %err, "failed to run unwind");
                        })?;

                        if clear_partial_trie_unwind {
                            let provider_rw = factory.database_provider_rw()?;
                            delete_partial_trie_unwind_marker(&provider_rw)?;
                            provider_rw.commit()?;
                        }
                    }
                    Ok(())
                }
                .await;
                let _ = tx.send(result);
            });
            rx.await??;
        }

        Ok(factory)
    }

    /// Opens and checks storage, returning the provider stage of startup.
    pub async fn with_provider_factory(
        self,
        database: &base_execution_state_database::DatabaseEnv,
        overlay_manager: OverlayManager,
        disabled_stages: &[StageId],
    ) -> eyre::Result<ProviderLaunch> {
        let provider_factory =
            self.create_provider_factory(database, overlay_manager, disabled_stages).await?;
        Ok(ProviderLaunch { configured: self, provider_factory })
    }
}

/// Checked storage ready for genesis initialization and component assembly.
#[derive(Debug)]
pub struct ProviderLaunch {
    /// Loaded configuration and process resources.
    pub configured: ConfiguredLaunch,
    /// Factory for the checked node storage.
    pub provider_factory: ProviderFactory,
}

impl ProviderLaunch {
    /// Starts the prometheus endpoint.
    pub async fn start_prometheus_endpoint(&self) -> eyre::Result<()> {
        // ensure recorder runs upkeep periodically
        install_prometheus_recorder().spawn_upkeep();

        let listen_addr = self.configured.configs.config.metrics.prometheus;
        if let Some(addr) = listen_addr {
            let prune_config = self.configured.prune_config();
            let pruning_mode = PruneConfigKind::from_config(
                &prune_config,
                self.configured.configs.config.chain.as_ref(),
            )
            .as_str();
            // On existing databases, stored settings are authoritative and already cached by the
            // provider factory. Fresh databases do not have storage metadata until genesis is
            // initialized, so report the configured setting during this pre-genesis startup window.
            let _storage_settings =
                if self.provider_factory.get_stage_checkpoint(StageId::Headers)?.is_some() {
                    self.provider_factory.cached_storage_settings()
                } else {
                    self.configured.configs.config.storage_settings()
                };
            let config = MetricServerConfig::new(
                addr,
                VersionInfo { version: version_metadata().cargo_pkg_version.as_ref() },
                ChainSpecInfo { name: self.configured.configs.config.chain.chain().to_string() },
                self.configured.context.task_executor.clone(),
                metrics_hooks(&self.provider_factory),
                self.configured.context.data_dir.pprof_dumps(),
            )
            .with_storage_settings_info(StorageSettingsInfo {
                storage_v2: true,
                pruning_mode,
                prune_config: serde_json::to_string(&prune_config)
                    .expect("serializing PruneConfig should not fail"),
            })
            .with_push_gateway(
                self.configured.configs.config.metrics.push_gateway_url.clone(),
                self.configured.configs.config.metrics.push_gateway_interval,
            );

            MetricServer::new(config).serve().await?;
        }

        Ok(())
    }

    /// Convenience function to [`Self::init_genesis`]
    pub fn with_genesis(self) -> Result<Self, InitStorageError> {
        init_genesis_with_settings_and_validate(
            &self.provider_factory,
            self.configured.configs.config.storage_settings(),
            !self.configured.configs.config.debug.skip_genesis_validation,
        )?;
        Ok(self)
    }

    /// Write the genesis block and state if it has not already been written
    pub fn init_genesis(&self) -> Result<B256, InitStorageError> {
        init_genesis_with_settings(
            &self.provider_factory,
            self.configured.configs.config.storage_settings(),
        )
    }

    /// Starts stage metrics, opens the blockchain provider, and builds node components.
    pub async fn with_components(
        self,
        base: &BaseNode,
        payload: Option<crate::BasePayloadServiceConfig>,
    ) -> eyre::Result<ComponentLaunch> {
        let (metrics_sender, metrics_receiver) = unbounded_channel();
        debug!(target: "reth::cli", "Spawning stages metrics listener task");
        let sync_metrics_listener = base_execution_sync::MetricsListener::new(metrics_receiver);
        self.configured
            .context
            .task_executor
            .spawn_critical_task("stages metrics listener task", sync_metrics_listener);

        let blockchain_db = BlockchainProvider::new(self.provider_factory.clone())?;
        let head = self
            .configured
            .configs
            .config
            .lookup_head(&self.provider_factory)
            .wrap_err("the head block is missing")?;
        let builder_ctx = BuilderContext::new(
            head,
            blockchain_db,
            self.configured.context.task_executor.clone(),
            self.configured.configs.clone(),
        );

        debug!(target: "reth::cli", "creating components");
        let node = base.build_components(&builder_ctx, payload).await?;
        Ok(ComponentLaunch {
            configured: self.configured,
            provider_factory: self.provider_factory,
            metrics_sender,
            node,
            head,
        })
    }
}

/// Assembled components ready for engine and RPC startup.
#[derive(Debug)]
pub struct ComponentLaunch {
    /// Loaded configuration and process resources.
    pub configured: ConfiguredLaunch,
    /// Factory for the node storage.
    pub provider_factory: ProviderFactory,
    /// Sender for stage metrics.
    pub metrics_sender: UnboundedSender<MetricEvent>,
    /// Built node services and providers.
    pub node: BaseNodeContext,
    /// Head read before component assembly.
    pub head: Head,
}

impl ComponentLaunch {
    /// Returns the max block that the node should run to, looking it up from the network if
    /// necessary
    pub async fn max_block<C>(&self, client: C) -> eyre::Result<Option<BlockNumber>>
    where
        C: HeadersClient,
    {
        self.configured.configs.config.max_block(client, self.provider_factory.clone()).await
    }

    /// Creates a new [`StaticFileProducer`] for the node storage.
    pub fn static_file_producer(&self) -> StaticFileProducer<ProviderFactory> {
        StaticFileProducer::new(self.provider_factory.clone(), self.configured.prune_modes())
    }

    /// Returns the initial backfill to sync to at launch.
    ///
    /// This returns the configured `debug.tip` if set, otherwise it will check if backfill was
    /// previously interrupted and returns the block hash of the last checkpoint, see also
    /// [`Self::check_pipeline_consistency`]
    pub fn initial_backfill_target(
        &self,
        disabled_stages: &[StageId],
    ) -> ProviderResult<Option<B256>> {
        let mut initial_target = self.configured.configs.config.debug.tip;

        if initial_target.is_none() {
            initial_target = self.check_pipeline_consistency(disabled_stages)?;
        }

        Ok(initial_target)
    }

    /// Returns true if the node should terminate after the initial backfill run.
    ///
    /// This is the case if any of these configs are set:
    ///  `--debug.max-block`
    ///  `--debug.terminate`
    pub const fn terminate_after_initial_backfill(&self) -> bool {
        self.configured.configs.config.debug.terminate
            || self.configured.configs.config.debug.max_block.is_some()
    }

    /// Check if the pipeline is consistent (all stages have the checkpoint block numbers no less
    /// than the checkpoint of the first stage).
    ///
    /// This will return the pipeline target if:
    ///  * the pipeline was interrupted during its previous run
    ///  * a new stage was added
    ///  * stage data was dropped manually through `reth stage drop ...`
    ///
    /// # Returns
    ///
    /// A target block hash if the pipeline is inconsistent, otherwise `None`.
    pub fn check_pipeline_consistency(
        &self,
        disabled_stages: &[StageId],
    ) -> ProviderResult<Option<B256>> {
        let mut all_stages = StageId::ALL.into_iter().filter(|id| !disabled_stages.contains(id));

        // Get the expected first stage based on config.
        let first_stage = all_stages.next().expect("there must be at least one stage");

        // If no target was provided, check if the stages are congruent - check if the
        // checkpoint of the last stage matches the checkpoint of the first.
        let first_stage_checkpoint =
            self.node.provider.get_stage_checkpoint(first_stage)?.unwrap_or_default().block_number;

        // Compare all other stages against the first
        for stage_id in all_stages {
            let stage_checkpoint =
                self.node.provider.get_stage_checkpoint(stage_id)?.unwrap_or_default().block_number;

            // If the checkpoint of any stage is less than the checkpoint of the first stage,
            // retrieve and return the block hash of the latest header and use it as the target.
            debug!(
                target: "consensus::engine",
                first_stage_id = %first_stage,
                first_stage_checkpoint,
                stage_id = %stage_id,
                stage_checkpoint = stage_checkpoint,
                "Checking stage against first stage",
            );
            if stage_checkpoint < first_stage_checkpoint {
                debug!(
                    target: "consensus::engine",
                    first_stage_id = %first_stage,
                    first_stage_checkpoint,
                    inconsistent_stage_id = %stage_id,
                    inconsistent_stage_checkpoint = stage_checkpoint,
                    "Pipeline sync progress is inconsistent"
                );
                return self.node.provider.block_hash(first_stage_checkpoint);
            }
        }

        Ok(None)
    }

    /// Creates an [`ExExLauncher`] for the installed execution extensions.
    ///
    /// This returns the launcher before calling `.launch()`, allowing custom configuration
    /// such as setting the WAL blocks warning threshold for L2 chains with faster block times:
    ///
    /// ```ignore
    /// ctx.exex_launcher(exexes)
    ///     .with_wal_blocks_warning(768)  // For 2-second block times
    ///     .launch()
    ///     .await
    /// ```
    pub fn exex_launcher(&self, installed_exex: Vec<crate::BaseExecutionService>) -> ExExLauncher {
        ExExLauncher::new(
            self.head,
            self.node.clone(),
            installed_exex,
            self.configured.configs.clone(),
        )
    }

    /// Creates consensus layer health events stream based on node configuration.
    ///
    /// Returns a stream that monitors consensus layer health if:
    /// - No debug tip is configured
    /// - Not running in dev mode
    ///
    /// Otherwise returns an empty stream.
    pub fn consensus_layer_events(&self) -> impl Stream<Item = NodeEvent> + 'static
    where
        BlockchainProvider: base_execution_state_provider::CanonChainTracker,
    {
        if self.configured.configs.config.debug.tip.is_none()
            && !self.configured.configs.config.dev.dev
        {
            Either::Left(
                ConsensusLayerHealthEvents::new(Box::new(self.node.provider.clone()))
                    .map(Into::into),
            )
        } else {
            Either::Right(stream::empty())
        }
    }

    /// Spawns the [`EthStatsService`] service if configured.
    pub async fn spawn_ethstats<St>(&self, mut engine_events: St) -> eyre::Result<()>
    where
        St: Stream<Item = base_common_types_payload::ConsensusEngineEvent> + Send + Unpin + 'static,
    {
        let Some(url) = self.configured.configs.config.debug.ethstats.as_ref() else {
            return Ok(());
        };

        let network = self.node.network().clone();
        let pool = self.node.pool().clone();
        let provider = self.node.provider.clone();

        info!(target: "reth::cli", %url, "Starting EthStats service");

        let ethstats = EthStatsService::new(url, network, provider, pool).await?;

        // If engine events are provided, spawn listener for new payload reporting
        let ethstats_for_events = ethstats.clone();
        let task_executor = self.configured.context.task_executor.clone();
        task_executor.spawn_task(async move {
            while let Some(event) = engine_events.next().await {
                match event {
                    ConsensusEngineEvent::ForkBlockAdded(executed, duration)
                    | ConsensusEngineEvent::CanonicalBlockAdded(executed, duration) => {
                        let block_hash = executed.recovered_block.num_hash().hash;
                        let block_number = executed.recovered_block.num_hash().number;
                        if let Err(e) = ethstats_for_events
                            .report_new_payload(block_hash, block_number, duration)
                            .await
                        {
                            debug!(
                                target: "ethstats",
                                error = %e, "Failed to report new payload"
                            );
                        }
                    }
                    _ => {
                        // Ignore other event types for ethstats reporting
                    }
                }
            }
        });

        // Spawn main ethstats service
        task_executor.spawn_task(async move { ethstats.run().await });

        Ok(())
    }
}

/// Helper container type to bundle the initial [`NodeConfig`] and the loaded settings from the
/// reth.toml config
#[derive(Debug)]
pub struct WithConfigs {
    /// The configured, usually derived from the CLI.
    pub config: NodeConfig,
    /// The loaded reth.toml config.
    pub toml_config: base_node_config::NodeFileConfig,
}

impl Clone for WithConfigs {
    fn clone(&self) -> Self {
        Self { config: self.config.clone(), toml_config: self.toml_config.clone() }
    }
}

/// Returns the metrics hooks for the node.
pub fn metrics_hooks(provider_factory: &ProviderFactory) -> Hooks {
    Hooks::builder()
        .with_hook({
            let db = provider_factory.db_ref().clone();
            move || throttle!(Duration::from_secs(5 * 60), || db.report_metrics())
        })
        .with_hook({
            let sfp = provider_factory.static_file_provider();
            move || {
                throttle!(Duration::from_secs(5 * 60), || {
                    if let Err(error) = sfp.report_metrics() {
                        error!(%error, "Failed to report metrics from static file provider");
                    }
                })
            }
        })
        .with_hook({
            let rocksdb = provider_factory.rocksdb_provider();
            move || throttle!(Duration::from_secs(5 * 60), || rocksdb.report_metrics())
        })
        .build()
}

fn get_partial_trie_unwind_marker(
    provider: &(impl MetadataProvider + StageCheckpointReader),
) -> ProviderResult<(Option<PartialStateTrieUnwindMarker>, bool)> {
    if let Some(marker) = provider.get_metadata(PARTIAL_STATE_TRIE_UNWIND_METADATA_KEY)? {
        let marker = serde_json::from_slice::<PartialStateTrieUnwindMarker>(&marker)
            .map_err(ProviderError::other)?;
        if marker.partial_state_trie >= marker.finish_block_number {
            return Err(ProviderError::other(std::io::Error::other(format!(
                "partial state trie unwind target #{} is not below original Finish #{}",
                marker.partial_state_trie, marker.finish_block_number,
            ))));
        }
        return Ok((Some(marker), true));
    }

    let Some(finish_checkpoint) = provider.get_stage_checkpoint(StageId::Finish)? else {
        return Ok((None, false));
    };
    let Some(partial_state_trie) =
        finish_checkpoint.finish_stage_checkpoint().and_then(|finish| finish.partial_state_trie())
    else {
        return Ok((None, false));
    };

    if partial_state_trie > finish_checkpoint.block_number {
        return Err(ProviderError::other(std::io::Error::other(format!(
            "partial state trie frontier #{partial_state_trie} is ahead of Finish #{}",
            finish_checkpoint.block_number,
        ))));
    }

    Ok((
        (partial_state_trie < finish_checkpoint.block_number).then_some(
            PartialStateTrieUnwindMarker {
                finish_block_number: finish_checkpoint.block_number,
                partial_state_trie,
            },
        ),
        false,
    ))
}

/// Metadata key for a partial state trie unwind that has not completed yet.
const PARTIAL_STATE_TRIE_UNWIND_METADATA_KEY: &str = "partial_state_trie_unwind";

fn write_partial_trie_unwind_marker(
    provider: &base_execution_state_provider::DatabaseProvider<
        impl base_execution_state_database::DbTxMut,
    >,
    marker: PartialStateTrieUnwindMarker,
) -> ProviderResult<()> {
    provider.write_metadata(
        PARTIAL_STATE_TRIE_UNWIND_METADATA_KEY,
        serde_json::to_vec(&marker).map_err(ProviderError::other)?,
    )
}

fn delete_partial_trie_unwind_marker(
    provider: &base_execution_state_provider::DatabaseProvider<
        impl base_execution_state_database::DbTxMut,
    >,
) -> ProviderResult<()> {
    provider.delete_metadata(PARTIAL_STATE_TRIE_UNWIND_METADATA_KEY)
}

#[cfg(test)]
mod tests {
    use base_execution_state_database::models::PartialStateTrieUnwindMarker;
    use base_execution_state_provider::{MetadataProvider, ProviderResult, StageCheckpointReader};
    use base_execution_sync::{FinishCheckpoint, StageCheckpoint, StageId};
    use base_node_config::{NodeFileConfig as Config, PruningArgs};

    use super::{LaunchContext, NodeConfig, get_partial_trie_unwind_marker};

    const EXTENSION: &str = "toml";

    struct MockProvider(Option<Vec<u8>>, Option<StageCheckpoint>);

    impl MetadataProvider for MockProvider {
        fn get_metadata(&self, _: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(self.0.clone())
        }
    }

    impl StageCheckpointReader for MockProvider {
        fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
            assert_eq!(id, StageId::Finish);
            Ok(self.1)
        }

        fn get_stage_checkpoint_progress(&self, _: StageId) -> ProviderResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
            Ok(Vec::new())
        }
    }

    fn with_tempdir(filename: &str, proc: fn(&std::path::Path)) {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join(filename).with_extension(EXTENSION);
        proc(&config_path);
        temp_dir.close().unwrap()
    }

    #[test]
    fn test_save_prune_config() {
        with_tempdir("prune-store-test", |config_path| {
            let mut reth_config = Config::default();
            let node_config = NodeConfig {
                pruning: PruningArgs {
                    full: true,
                    minimal: false,
                    block_interval: None,
                    sender_recovery_full: false,
                    sender_recovery_distance: None,
                    sender_recovery_before: None,
                    transaction_lookup_full: false,
                    transaction_lookup_distance: None,
                    transaction_lookup_before: None,
                    receipts_full: false,
                    receipts_pre_merge: false,
                    receipts_distance: None,
                    receipts_before: None,
                    account_history_full: false,
                    account_history_distance: None,
                    account_history_before: None,
                    storage_history_full: false,
                    storage_history_distance: None,
                    storage_history_before: None,
                    bodies_pre_merge: false,
                    bodies_distance: None,
                    receipts_log_filter: None,
                    bodies_before: None,
                    minimum_distance: None,
                },
                ..NodeConfig::test()
            };
            LaunchContext::save_pruning_config(&mut reth_config, &node_config, config_path)
                .unwrap();

            let loaded_config = Config::from_path(config_path).unwrap();

            assert_eq!(reth_config, loaded_config);
        })
    }

    #[test]
    fn get_partial_trie_unwind_marker_uses_partial_finish_checkpoint() {
        let finish_checkpoint = StageCheckpoint::new(42)
            .with_finish_stage_checkpoint(FinishCheckpoint { partial_state_trie: Some(21) });
        let expected =
            finish_checkpoint.finish_stage_checkpoint().unwrap().partial_state_trie().map(
                |partial_state_trie| PartialStateTrieUnwindMarker {
                    finish_block_number: finish_checkpoint.block_number,
                    partial_state_trie,
                },
            );

        assert_eq!(
            get_partial_trie_unwind_marker(&MockProvider(None, Some(finish_checkpoint))).unwrap(),
            (expected, false)
        );

        let genesis_checkpoint = StageCheckpoint::new(42)
            .with_finish_stage_checkpoint(FinishCheckpoint { partial_state_trie: Some(0) });
        let expected =
            genesis_checkpoint.finish_stage_checkpoint().unwrap().partial_state_trie().map(
                |partial_state_trie| PartialStateTrieUnwindMarker {
                    finish_block_number: genesis_checkpoint.block_number,
                    partial_state_trie,
                },
            );

        assert_eq!(
            get_partial_trie_unwind_marker(&MockProvider(None, Some(genesis_checkpoint))).unwrap(),
            (expected, false)
        );
    }

    #[test]
    fn get_partial_trie_unwind_marker_resumes_persisted_unwind() {
        let marker =
            PartialStateTrieUnwindMarker { finish_block_number: 42, partial_state_trie: 21 };

        assert_eq!(
            get_partial_trie_unwind_marker(&MockProvider(
                Some(serde_json::to_vec(&marker).unwrap()),
                Some(StageCheckpoint::new(21)),
            ),)
            .unwrap(),
            (Some(marker), true)
        );
        assert_eq!(
            get_partial_trie_unwind_marker(&MockProvider(
                Some(serde_json::to_vec(&marker).unwrap()),
                None
            ),)
            .unwrap(),
            (Some(marker), true)
        );
    }

    #[test]
    fn get_partial_trie_unwind_marker_ignores_non_lagging_or_missing_partial_checkpoint() {
        let matching_finish_checkpoint = StageCheckpoint::new(42)
            .with_finish_stage_checkpoint(FinishCheckpoint { partial_state_trie: Some(42) });
        let ahead_finish_checkpoint = StageCheckpoint::new(42)
            .with_finish_stage_checkpoint(FinishCheckpoint { partial_state_trie: Some(43) });
        let missing_partial_finish_checkpoint = StageCheckpoint::new(42)
            .with_finish_stage_checkpoint(FinishCheckpoint { partial_state_trie: None });

        assert_eq!(
            get_partial_trie_unwind_marker(&MockProvider(None, Some(matching_finish_checkpoint)),)
                .unwrap(),
            (None, false)
        );
        assert_eq!(
            get_partial_trie_unwind_marker(&MockProvider(
                None,
                Some(missing_partial_finish_checkpoint)
            ),)
            .unwrap(),
            (None, false)
        );
        assert_eq!(
            get_partial_trie_unwind_marker(&MockProvider(None, None)).unwrap(),
            (None, false)
        );

        let partial_frontier = ahead_finish_checkpoint
            .finish_stage_checkpoint()
            .and_then(|finish| finish.partial_state_trie());
        let result =
            get_partial_trie_unwind_marker(&MockProvider(None, Some(ahead_finish_checkpoint)));
        if partial_frontier.is_some() {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("ahead of Finish"), "unexpected error: {error}");
        } else {
            assert_eq!(result.unwrap(), (None, false));
        }
    }

    #[test]
    fn get_partial_trie_unwind_marker_rejects_invalid_persisted_marker() {
        let marker =
            PartialStateTrieUnwindMarker { finish_block_number: 42, partial_state_trie: 42 };
        let error = get_partial_trie_unwind_marker(&MockProvider(
            Some(serde_json::to_vec(&marker).unwrap()),
            None,
        ))
        .unwrap_err();

        assert!(error.to_string().contains("is not below original Finish"));
    }

    #[test]
    fn get_partial_trie_unwind_marker_rejects_malformed_metadata() {
        assert!(get_partial_trie_unwind_marker(&MockProvider(Some(vec![0xff]), None)).is_err());
    }
}
