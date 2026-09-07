//! Engine orchestrator launch helper.
//!
//! Provides [`build_engine_orchestrator`](crate::launch::build_engine_orchestrator) which wires
//! together all engine components and returns a
//! [`ChainOrchestrator`](crate::chain::ChainOrchestrator) ready to be polled as a `Stream`.

use std::sync::Arc;

use base_common_consensus::BaseBlock;
use futures::Stream;
use reth_consensus::FullConsensus;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_engine_primitives::BeaconEngineMessage;
use reth_evm::BaseEvmConfig;
use reth_network_p2p::BlockClient;
use reth_payload_builder::PayloadBuilderHandle;
use reth_provider::{ProviderFactory, providers::BlockchainProvider};
use reth_prune::PrunerWithFactory;
use reth_stages_api::{MetricEventsSender, Pipeline};
use reth_storage_overlay::OverlayManager;
use reth_tasks::Runtime;

use crate::{
    backfill::PipelineSync,
    chain::ChainOrchestrator,
    download::BasicBlockDownloader,
    engine::{EngineApiKind, EngineApiRequest, EngineApiRequestHandler, EngineHandler},
    persistence::PersistenceHandle,
    tree::{EngineApiTreeHandler, EngineValidator, TreeConfig, WaitForCaches},
};

/// Builds the engine [`ChainOrchestrator`] that drives the chain forward.
///
/// This spawns and wires together the following components:
///
/// - **[`BasicBlockDownloader`]** — downloads blocks on demand from the network during live sync.
/// - **[`PersistenceHandle`]** — spawns the persistence service on a background thread for writing
///   blocks and performing pruning outside the critical consensus path.
/// - **[`EngineApiTreeHandler`]** — spawns the tree handler that processes engine API requests
///   (`newPayload`, `forkchoiceUpdated`) and maintains the in-memory chain state.
/// - **[`EngineApiRequestHandler`]** + **[`EngineHandler`]** — glue that routes incoming CL
///   messages to the tree handler and manages download requests.
/// - **[`PipelineSync`]** — wraps the staged sync [`Pipeline`] for backfill sync when the node
///   needs to catch up over large block ranges.
///
/// The returned orchestrator implements [`Stream`] and yields
/// [`ChainEvent`]s.
///
/// [`ChainEvent`]: crate::chain::ChainEvent
#[expect(clippy::too_many_arguments, clippy::type_complexity)]
pub fn build_engine_orchestrator<DB, Client, S, V>(
    engine_kind: EngineApiKind,
    consensus: Arc<dyn FullConsensus>,
    client: Client,
    incoming_requests: S,
    pipeline: Pipeline<DB>,
    pipeline_task_spawner: Runtime,
    provider: ProviderFactory<DB>,
    blockchain_db: BlockchainProvider<DB>,
    pruner: PrunerWithFactory<ProviderFactory<DB>>,
    payload_builder: PayloadBuilderHandle,
    payload_validator: V,
    overlay_manager: OverlayManager,
    tree_config: TreeConfig,
    sync_metrics_tx: MetricEventsSender,
    evm_config: BaseEvmConfig,
    runtime: Runtime,
) -> ChainOrchestrator<
    EngineHandler<EngineApiRequestHandler<EngineApiRequest>, S, BasicBlockDownloader<Client>>,
    PipelineSync<DB>,
>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    Client: BlockClient<Block = BaseBlock> + 'static,
    S: Stream<Item = BeaconEngineMessage> + Send + Sync + Unpin + 'static,
    V: EngineValidator + WaitForCaches,
{
    let downloader = BasicBlockDownloader::new(client, consensus.clone());

    let persistence_handle = PersistenceHandle::spawn_service(provider, pruner, sync_metrics_tx);

    let canonical_in_memory_state = blockchain_db.canonical_in_memory_state();

    let (to_tree_tx, from_tree) = EngineApiTreeHandler::spawn_new(
        blockchain_db,
        consensus,
        payload_validator,
        persistence_handle,
        payload_builder,
        canonical_in_memory_state,
        overlay_manager,
        tree_config,
        engine_kind,
        evm_config,
        runtime,
    );

    let engine_handler = EngineApiRequestHandler::new(to_tree_tx, from_tree);
    let handler = EngineHandler::new(engine_handler, downloader, incoming_requests);

    let backfill_sync = PipelineSync::new(pipeline, pipeline_task_spawner);

    ChainOrchestrator::new(handler, backfill_sync)
}
