//! Engine node related functionality.

use base_common_types_chain::BlockHeader;
use base_execution_payload_builder::{BaseEngineValidator, BaseExecutionHandle};
use base_node_context::AddOnsContext;
use futures::{FutureExt, StreamExt, stream::FusedStream, stream_select};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_engine_tree::{
    chain::{ChainEvent, FromOrchestrator},
    engine::{EngineApiKind, EngineApiRequest},
    launch::build_engine_orchestrator,
};
use reth_engine_util::EngineMessageStreamExt;
use reth_exex::ExExManagerHandle;
use reth_network::{NetworkSyncUpdater, SyncState, types::BlockRangeUpdate};
use reth_network_api::BlockDownloaderProvider;
use reth_node_core::{args::PruneConfigKind, exit::NodeExitFuture, primitives::Head};
use reth_node_events::node;
use base_execution_state_provider::{BlockNumReader, StorageSettingsCache};
use reth_storage_overlay::OverlayManager;
use base_common_runtime_tasks::EventSender;
use base_common_observability_tracing::tracing::{debug, error, info};
use tokio::sync::{mpsc::unbounded_channel, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    EngineShutdown, FullNode, LaunchContext, NodeHandle,
    rpc::{BasicEngineValidatorBuilder, RpcHandle},
    setup::build_networked_pipeline,
};

impl crate::NodeLaunch {
    /// Starts Base execution, canonical processing, and the built-in RPC servers.
    pub async fn launch(self) -> eyre::Result<NodeHandle> {
        let ctx = LaunchContext::new(self.task_executor, self.config.datadir());
        let engine_tree_config = self.engine_tree_config;
        let database = self.database;
        let config = self.config;
        let base = self.base;
        let rpc = self.rpc;
        let payload = self.payload;
        let mut services = self.services.prepare(&base.args)?;

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
            .with_provider_factory(overlay_manager.clone(), disabled_stages)
            .await?;
        info!(target: "reth::cli", "Database opened");
        let ctx = ctx.with_prometheus_server().await?;
        debug!(target: "reth::cli", chain=%ctx.chain_id(), genesis=?ctx.genesis_hash(), "Initializing genesis");
        let ctx = ctx.with_genesis()?;
        info!(target: "reth::cli", hardforks=%ctx.chain_spec().display_hardforks(), "Loaded hardfork schedule");
        let settings = ctx.provider_factory().cached_storage_settings();
        let pruning_mode =
            PruneConfigKind::from_config(&ctx.prune_config(), ctx.chain_spec().as_ref()).as_str();
        info!(target: "reth::cli", ?settings, ?pruning_mode, "Loaded storage settings");
        let ctx =
            ctx.with_metrics_task().with_blockchain_db()?.with_components(&base, payload).await?;

        services.start_tracing(ctx.node_adapter());

        // spawn the configured canonical processors
        let maybe_exex_manager_handle = ctx.launch_exex(services.execution_services()).await?;

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

        let add_ons_ctx = AddOnsContext {
            node: ctx.node_adapter().clone(),
            config: ctx.node_config(),
            beacon_engine_handle: beacon_engine_handle.clone(),
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
        let consensus_engine_stream = UnboundedReceiverStream::from(consensus_engine_rx)
            .maybe_skip_fcu(node_config.debug.skip_fcu)
            .maybe_skip_new_payload(node_config.debug.skip_new_payload)
            .maybe_reorg(
                ctx.blockchain_db().clone(),
                ctx.node_adapter().evm_config().clone(),
                node_config.debug.reorg_frequency,
                node_config.debug.reorg_depth,
            )
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
            crate::BaseRpcServer::launch(add_ons_ctx, &base, rpc, &services).await?;

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
                            orchestrator.handler_mut().on_event(EngineApiRequest::InsertExecutedBlock(executed_block).into());
                        }
                    }
                    shutdown_req = &mut shutdown_rx => {
                        if let Ok(req) = shutdown_req {
                            debug!(target: "reth::cli", "received engine shutdown request");
                            orchestrator.handler_mut().on_event(
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
                        orchestrator.handler_mut().on_event(
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
                validator: BaseEngineValidator::new(
                    ctx.node_adapter().evm_config().chain_spec().clone(),
                ),
            },
            engine_events,
            engine_shutdown,
            proofs_progress: Default::default(),
            task_executor: ctx.task_executor().clone(),
            config: ctx.node_config().clone(),
            data_dir: ctx.data_dir().clone(),
            add_ons_handle: RpcHandle { rpc_server_handles, rpc_registry },
        };
        services.start(&full_node)?;

        ctx.spawn_ethstats(engine_events_for_ethstats).await?;

        let handle = NodeHandle {
            node_exit_future: NodeExitFuture::new(async { rx.await? }),
            node: full_node,
        };

        crate::BaseDebugServices::start(&handle).await?;
        Ok(handle)
    }
}
