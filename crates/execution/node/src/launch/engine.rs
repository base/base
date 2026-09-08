//! Engine node related functionality.

use std::{future::Future, pin::Pin};

use base_common_consensus::BlockHeader;
use base_execution_payload_builder::{BaseEngineValidator, BaseExecutionHandle};
use base_execution_payload_types::BuiltPayload;
use base_node_context::{AddOnsContext, BaseNodeContext, FullNodeComponents};
use futures::{FutureExt, StreamExt, stream::FusedStream, stream_select};
use reth_db::{Database, database_metrics::DatabaseMetrics};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_engine_tree::{
    chain::{ChainEvent, FromOrchestrator},
    engine::{EngineApiKind, EngineApiRequest, EngineRequestHandler},
    launch::build_engine_orchestrator,
    tree::TreeConfig,
};
use reth_engine_util::EngineMessageStreamExt;
use reth_exex::ExExManagerHandle;
use reth_network::{NetworkSyncUpdater, SyncState, types::BlockRangeUpdate};
use reth_network_api::BlockDownloaderProvider;
use reth_node_core::{
    args::PruneConfigKind,
    dirs::{ChainPath, DataDirPath},
    exit::NodeExitFuture,
    primitives::Head,
};
use reth_node_events::node;
use reth_provider::{BlockNumReader, StorageSettingsCache, providers::BlockchainProvider};
use reth_storage_overlay::OverlayManager;
use reth_tasks::TaskExecutor;
use reth_tokio_util::EventSender;
use reth_tracing::tracing::{debug, error, info};
use reth_trie_common::KeccakKeyHasher;
use tokio::sync::{mpsc::unbounded_channel, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    Attached, EngineShutdown, FullNode, LaunchContext, LaunchContextWith, LaunchNode,
    NodeBuilderWithComponents, NodeHandle, WithConfigs,
    hooks::NodeHooks,
    rpc::{BasicEngineValidatorBuilder, RethRpcAddOns, RpcHandle},
    setup::build_networked_pipeline,
};

/// The engine node launcher.
#[derive(Debug)]
pub struct EngineNodeLauncher {
    /// The task executor for the node.
    pub ctx: LaunchContext,

    /// Temporary configuration for engine tree.
    /// After engine is stabilized, this should be configured through node builder.
    pub engine_tree_config: TreeConfig,
}

impl EngineNodeLauncher {
    /// Create a new instance of the ethereum node launcher.
    pub const fn new(
        task_executor: TaskExecutor,
        data_dir: ChainPath<DataDirPath>,
        engine_tree_config: TreeConfig,
    ) -> Self {
        Self { ctx: LaunchContext::new(task_executor, data_dir), engine_tree_config }
    }

    async fn launch_node<DB, AO>(
        self,
        target: NodeBuilderWithComponents<DB, AO>,
    ) -> eyre::Result<NodeHandle<BaseNodeContext<DB>, AO>>
    where
        DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
        AO: RethRpcAddOns<BaseNodeContext<DB>>,
    {
        let Self { ctx, engine_tree_config } = self;
        let NodeBuilderWithComponents {
            database,
            rocksdb_provider,
            components_builder,
            hooks,
            exexs: installed_exex,
            add_ons,
            config,
        } = target;
        let NodeHooks { on_component_initialized, on_node_started, .. } = hooks;

        // Create the overlay manager that will be shared across the provider and engine.
        let overlay_manager =
            OverlayManager::new(ctx.task_executor.state_trie_overlay_worker_pool());
        let disabled_stages = &[];

        // setup the launch context
        let ctx = ctx
            .with_configured_globals(engine_tree_config.reserved_cpu_cores())
            // load the toml config
            .with_loaded_toml_config(config)?
            // add resolved peers
            .with_resolved_peers()?
            // attach the database
            .attach(database.clone())
            // ensure certain settings take effect
            .with_adjusted_configs()
            // Create the provider factory with the shared overlay manager
            .with_provider_factory(
                overlay_manager.clone(),
                rocksdb_provider,
                disabled_stages,
            )
            .await?
            .inspect(|_| {
                info!(target: "reth::cli", "Database opened");
            })
            .with_prometheus_server().await?
            .inspect(|this| {
                debug!(target: "reth::cli", chain=%this.chain_id(), genesis=?this.genesis_hash(), "Initializing genesis");
            })
            .with_genesis()?
            .inspect(|this: &LaunchContextWith<Attached<WithConfigs, _>>| {
                info!(target: "reth::cli", "\n{}", this.chain_spec().display_hardforks());
                let settings = this.provider_factory().cached_storage_settings();
                let pruning_mode =
                    PruneConfigKind::from_config(&this.prune_config(), this.chain_spec().as_ref()).as_str();
                info!(target: "reth::cli", ?settings, ?pruning_mode, "Loaded storage settings");
            })
            .with_metrics_task()
            // later the components.
            .with_blockchain_db(move |provider_factory| {
                Ok(BlockchainProvider::new(provider_factory)?)
            })?
            .with_components(components_builder, on_component_initialized).await?;

        // spawn exexs if any
        let maybe_exex_manager_handle = ctx.launch_exex(installed_exex).await?;

        // create pipeline
        let network_handle = ctx.node_adapter().network().clone();
        let network_client = network_handle.fetch_client().await?;
        let (consensus_engine_tx, consensus_engine_rx) = unbounded_channel();

        let node_config = ctx.node_config();

        // We always assume that node is syncing after a restart
        network_handle.update_sync_state(SyncState::Syncing);

        let max_block = ctx.max_block(network_client.clone()).await?;

        let static_file_producer = ctx.static_file_producer();
        let static_file_producer_events = static_file_producer.lock().events();
        info!(target: "reth::cli", "StaticFileProducer initialized");

        let consensus = ctx.node_adapter().consensus().clone();

        let pipeline = build_networked_pipeline(
            &ctx.toml_config().stages,
            network_client.clone(),
            consensus.clone(),
            ctx.provider_factory().clone(),
            ctx.task_executor(),
            ctx.sync_metrics_tx(),
            ctx.prune_config(),
            max_block,
            static_file_producer,
            ctx.node_adapter().evm_config().clone(),
            maybe_exex_manager_handle.clone().unwrap_or_else(ExExManagerHandle::empty),
            disabled_stages,
        )?;

        let pipeline_events = pipeline.events();

        let mut pruner_builder = ctx.pruner_builder();
        if let Some(exex_manager_handle) = &maybe_exex_manager_handle {
            pruner_builder =
                pruner_builder.finished_exex_height(exex_manager_handle.finished_height());
        }
        let pruner = pruner_builder.build_with_provider_factory(ctx.provider_factory().clone());
        let pruner_events = pruner.events();
        info!(target: "reth::cli", prune_config=?ctx.prune_config(), "Pruner initialized");

        let event_sender = EventSender::default();

        let beacon_engine_handle = ConsensusEngineHandle::new(consensus_engine_tx.clone());

        // extract the jwt secret from the args if possible
        let jwt_secret = ctx.auth_jwt_secret()?;

        let add_ons_ctx = AddOnsContext {
            node: ctx.node_adapter().clone(),
            config: ctx.node_config(),
            beacon_engine_handle: beacon_engine_handle.clone(),
            jwt_secret,
            engine_events: event_sender.clone(),
        };

        // Build the engine validator with all required components
        let engine_validator = BasicEngineValidatorBuilder::build_tree_validator(
            &add_ons_ctx,
            engine_tree_config.clone(),
            overlay_manager.clone(),
        )
        .await?;

        // Create the consensus engine stream with optional reorg
        let reorg_overlay_manager = overlay_manager.clone();
        let consensus_engine_stream = UnboundedReceiverStream::from(consensus_engine_rx)
            .maybe_skip_fcu(node_config.debug.skip_fcu)
            .maybe_skip_new_payload(node_config.debug.skip_new_payload)
            .maybe_reorg(
                ctx.blockchain_db().clone(),
                ctx.node_adapter().evm_config().clone(),
                || async {
                    BasicEngineValidatorBuilder::build_tree_validator(
                        &add_ons_ctx,
                        engine_tree_config.clone(),
                        reorg_overlay_manager.clone(),
                    )
                    .await
                },
                node_config.debug.reorg_frequency,
                node_config.debug.reorg_depth,
            )
            .await?
            // Store messages _after_ skipping so that `replay-engine` command
            // would replay only the messages that were observed by the engine
            // during this run.
            .maybe_store_messages(node_config.debug.engine_api_store.clone());

        let engine_kind = EngineApiKind::OpStack;

        let mut orchestrator = build_engine_orchestrator(
            engine_kind,
            consensus.clone(),
            network_client.clone(),
            Box::pin(consensus_engine_stream),
            pipeline,
            ctx.task_executor().clone(),
            ctx.provider_factory().clone(),
            ctx.blockchain_db().clone(),
            pruner,
            ctx.node_adapter().payload_builder_handle().clone(),
            engine_validator,
            overlay_manager,
            engine_tree_config,
            ctx.sync_metrics_tx(),
            ctx.node_adapter().evm_config().clone(),
            ctx.task_executor().clone(),
        );

        info!(target: "reth::cli", "Consensus engine initialized");

        #[expect(clippy::needless_continue)]
        let events = stream_select!(
            event_sender.new_listener().map(Into::into),
            pipeline_events.map(Into::into),
            ctx.consensus_layer_events(),
            pruner_events.map(Into::into),
            static_file_producer_events.map(Into::into),
        );

        ctx.task_executor().spawn_critical_task(
            "events task",
            node::handle_events(
                Some(Box::new(ctx.node_adapter().network().clone())),
                Some(ctx.head().number),
                events,
            ),
        );

        let RpcHandle { rpc_server_handles, rpc_registry } =
            add_ons.launch_add_ons(add_ons_ctx).await?;

        // Create engine shutdown handle
        let (engine_shutdown, shutdown_rx) = EngineShutdown::new();

        // Run consensus engine to completion
        let initial_target = ctx.initial_backfill_target(disabled_stages)?;
        let mut built_payloads = ctx
            .node_adapter()
            .payload_builder_handle()
            .subscribe()
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to payload builder events: {:?}", e))?
            .into_built_payload_stream()
            .fuse();

        let provider = ctx.blockchain_db().clone();
        let (exit, rx) = oneshot::channel();
        let terminate_after_backfill = ctx.terminate_after_initial_backfill();
        let startup_sync_state_idle = ctx.node_config().debug.startup_sync_state_idle;

        info!(target: "reth::cli", "Starting consensus engine");
        let engine_events = event_sender.clone();
        let consensus_engine = move |mut on_graceful_shutdown| async move {
            if let Some(initial_target) = initial_target {
                debug!(target: "reth::cli", %initial_target,  "start backfill sync");
                // network_handle's sync state is already initialized at Syncing
                orchestrator.start_backfill_sync(initial_target);
            } else if startup_sync_state_idle {
                network_handle.update_sync_state(SyncState::Idle);
            }

            let mut res = Ok(());
            let mut shutdown_rx = shutdown_rx.fuse();

            // advance the chain and await payloads built locally to add into the engine api
            // tree handler to prevent re-execution if that block is received as payload from
            // the CL
            loop {
                tokio::select! {
                    event = orchestrator.next() => {
                        let Some(event) = event else { break };
                        debug!(target: "reth::cli", "Event: {event}");
                        match event {
                            ChainEvent::BackfillSyncFinished => {
                                if terminate_after_backfill {
                                    debug!(target: "reth::cli", "Terminating after initial backfill");
                                    break
                                }
                                if startup_sync_state_idle {
                                    network_handle.update_sync_state(SyncState::Idle);
                                }
                            }
                            ChainEvent::BackfillSyncStarted => {
                                network_handle.update_sync_state(SyncState::Syncing);
                            }
                            ChainEvent::FatalError => {
                                error!(target: "reth::cli", "Fatal error in consensus engine");
                                res = Err(eyre::eyre!("Fatal error in consensus engine"));
                                break
                            }
                            ChainEvent::Handler(ev) => {
                                if let Some(head) = ev.canonical_header() {
                                    // Once we're progressing via live sync, we can consider the node is not syncing anymore
                                    network_handle.update_sync_state(SyncState::Idle);
                                    let head_block = Head {
                                        number: head.number(),
                                        hash: head.hash(),
                                        difficulty: head.difficulty(),
                                        timestamp: head.timestamp(),
                                        total_difficulty: Default::default(),
                                    };
                                    network_handle.update_status(head_block);

                                    let updated = BlockRangeUpdate {
                                        earliest: provider.earliest_block_number().unwrap_or_default(),
                                        latest: head.number(),
                                        latest_hash: head.hash(),
                                    };
                                    network_handle.update_block_range(updated);
                                }
                                event_sender.notify(ev);
                            }
                        }
                    }
                    payload = built_payloads.select_next_some(), if !built_payloads.is_terminated() => {
                        if let Some(executed_block) = payload.executed_block() {
                            debug!(target: "reth::cli", block=?executed_block.recovered_block.num_hash(),  "inserting built payload");
                            orchestrator.handler_mut().handler_mut().on_event(EngineApiRequest::InsertExecutedBlock(executed_block).into());
                        }
                    }
                    shutdown_req = &mut shutdown_rx => {
                        if let Ok(req) = shutdown_req {
                            debug!(target: "reth::cli", "received engine shutdown request");
                            orchestrator.handler_mut().handler_mut().on_event(
                                FromOrchestrator::Terminate { tx: req.done_tx }.into()
                            );
                        }
                    }
                    _guard = &mut on_graceful_shutdown => {
                        // Shutdown signal received.
                        // Send Terminate so the engine OS thread can exit cleanly before we
                        // drop the orchestrator.
                        debug!(target: "reth::cli", "shutdown signal received, terminating engine");
                        let (done_tx, done_rx) = oneshot::channel();
                        orchestrator.handler_mut().handler_mut().on_event(
                            FromOrchestrator::Terminate { tx: done_tx }.into()
                        );
                        let _ = done_rx.await;
                        break;
                    }
                }
            }

            let _ = exit.send(res);
        };
        ctx.task_executor()
            .spawn_critical_with_graceful_shutdown_signal("consensus engine", consensus_engine);

        let engine_events_for_ethstats = engine_events.new_listener();

        let full_node = FullNode {
            evm_config: ctx.node_adapter().evm_config().clone(),
            pool: ctx.node_adapter().pool().clone(),
            network: ctx.node_adapter().network().clone(),
            provider: ctx.node_adapter().provider.clone(),
            payload_builder_handle: ctx.node_adapter().payload_builder_handle().clone(),
            execution: BaseExecutionHandle {
                driver: beacon_engine_handle,
                payload_builder: ctx.node_adapter().payload_builder_handle().clone(),
                validator: BaseEngineValidator::new::<KeccakKeyHasher>(
                    ctx.node_adapter().evm_config().chain_spec().clone(),
                ),
            },
            engine_events,
            engine_shutdown,
            task_executor: ctx.task_executor().clone(),
            config: ctx.node_config().clone(),
            data_dir: ctx.data_dir().clone(),
            add_ons_handle: RpcHandle { rpc_server_handles, rpc_registry },
        };
        // Notify on node started
        on_node_started.on_event(FullNode::clone(&full_node))?;

        ctx.spawn_ethstats(engine_events_for_ethstats).await?;

        let handle = NodeHandle {
            node_exit_future: NodeExitFuture::new(async { rx.await? }),
            node: full_node,
        };

        Ok(handle)
    }
}

impl<DB, AO> LaunchNode<NodeBuilderWithComponents<DB, AO>> for EngineNodeLauncher
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    AO: RethRpcAddOns<BaseNodeContext<DB>> + 'static,
{
    type Node = NodeHandle<BaseNodeContext<DB>, AO>;
    type Future = Pin<Box<dyn Future<Output = eyre::Result<Self::Node>> + Send>>;

    fn launch_node(self, target: NodeBuilderWithComponents<DB, AO>) -> Self::Future {
        Box::pin(self.launch_node(target))
    }
}
