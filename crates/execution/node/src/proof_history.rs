//! Node launcher with proof history support.

use std::{sync::Arc, time::Duration};

use base_execution_chainspec::BaseChainSpec;
use base_execution_exex::BaseProofsExEx;
use base_execution_rpc::{
    debug::{DebugApiExt, DebugApiOverrideServer},
    eth::proofs::{EthApiExt, EthApiOverrideServer},
};
use base_execution_trie::{
    BaseProofsBatchStore, BaseProofsStorage, MdbxProofsStorage, RocksdbProofsStorage,
};
use eyre::ErrReport;
use futures::FutureExt;
use reth_db::DatabaseEnv;
use reth_db_api::database_metrics::DatabaseMetrics;
use reth_node_builder::{
    FullNodeComponents, Node as RethNode, NodeBuilder, NodeBuilderWithComponents, RethFullAdapter,
    WithLaunchContext,
};
use reth_tasks::TaskExecutor;
use tokio::time::sleep;
use tracing::info;

use crate::{
    BaseNode, BaseNodeComponentBuilder,
    args::{DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS, ProofsHistoryDbBackend, RollupArgs},
};

type ProofHistoryNodeTypes = RethFullAdapter<Arc<DatabaseEnv>, BaseNode>;
type ProofHistoryNodeBuilder = WithLaunchContext<
    NodeBuilderWithComponents<
        ProofHistoryNodeTypes,
        BaseNodeComponentBuilder<ProofHistoryNodeTypes>,
        <BaseNode as RethNode<ProofHistoryNodeTypes>>::AddOns,
    >,
>;

/// - no proofs history (plain node),
/// - in-mem proofs storage,
/// - on-disk proofs storage.
pub async fn launch_node_with_proof_history(
    builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, BaseChainSpec>>,
    args: RollupArgs,
) -> eyre::Result<(), ErrReport> {
    let RollupArgs {
        sequencer,
        disable_txpool_gossip,
        compute_pending_block,
        discovery_v4,
        sequencer_headers,
        min_suggested_priority_fee,
        txpool_ordering,
        max_inflight_delegated_slots,
        proofs_history,
        proofs_history_storage_path,
        proofs_history_db,
        proofs_history_rocksdb,
        proofs_history_mdbx,
        proofs_history_window,
        proofs_history_prune_interval,
        proofs_history_verification_interval,
        upgrade_signal,
        upgrade_signal_l1_rpc,
    } = args;

    // Start from a plain BaseNode builder
    let mut node_builder = builder.node(BaseNode::new(RollupArgs {
        sequencer,
        disable_txpool_gossip,
        compute_pending_block,
        discovery_v4,
        sequencer_headers,
        min_suggested_priority_fee,
        txpool_ordering,
        max_inflight_delegated_slots,
        proofs_history: false,
        proofs_history_storage_path: None,
        proofs_history_db: ProofsHistoryDbBackend::default(),
        proofs_history_rocksdb: Default::default(),
        proofs_history_mdbx: Default::default(),
        proofs_history_window: DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS,
        proofs_history_prune_interval: Duration::from_secs(15),
        proofs_history_verification_interval: 0,
        upgrade_signal,
        upgrade_signal_l1_rpc,
    }));

    if proofs_history {
        let path = proofs_history_storage_path.ok_or_else(|| {
            eyre::eyre!("--proofs-history requires --proofs-history.storage-path")
        })?;
        info!(target: "reth::cli", "Using on-disk storage for proofs history");
        proofs_history_db.ensure_storage_path_matches(&path)?;

        match proofs_history_db {
            ProofsHistoryDbBackend::Rocksdb => {
                let rocksdb = Arc::new(
                    RocksdbProofsStorage::new_with_options(
                        &path,
                        proofs_history_rocksdb.storage_options()?,
                    )
                    .map_err(|e| eyre::eyre!("Failed to create RocksdbProofsStorage: {e}"))?,
                );
                node_builder = install_proofs_history(
                    node_builder,
                    rocksdb,
                    proofs_history_window,
                    proofs_history_prune_interval,
                    proofs_history_verification_interval,
                );
            }
            ProofsHistoryDbBackend::Mdbx => {
                let mdbx = Arc::new(
                    MdbxProofsStorage::new_with_options(
                        &path,
                        proofs_history_mdbx.storage_options(),
                    )
                    .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
                );
                node_builder = install_proofs_history(
                    node_builder,
                    mdbx,
                    proofs_history_window,
                    proofs_history_prune_interval,
                    proofs_history_verification_interval,
                );
            }
        }
    }

    // In all cases (with or without proofs), launch the node.
    let handle = node_builder.launch_with_debug_capabilities().await?;
    handle.node_exit_future.await
}

fn install_proofs_history<S>(
    node_builder: ProofHistoryNodeBuilder,
    storage_backend: Arc<S>,
    proofs_history_window: u64,
    proofs_history_prune_interval: Duration,
    proofs_history_verification_interval: u64,
) -> ProofHistoryNodeBuilder
where
    S: BaseProofsBatchStore + DatabaseMetrics + Send + Sync + 'static,
{
    let storage: BaseProofsStorage<Arc<S>> = Arc::clone(&storage_backend).into();
    let storage_exec = storage.clone();

    node_builder
        .on_node_started(move |node| {
            spawn_proofs_db_metrics(
                node.task_executor,
                storage_backend,
                node.config.metrics.push_gateway_interval,
            );
            Ok(())
        })
        .install_exex("proofs-history", async move |exex_context| {
            Ok(BaseProofsExEx::builder(exex_context, storage_exec)
                .with_proofs_history_window(proofs_history_window)
                .with_proofs_history_prune_interval(proofs_history_prune_interval)
                .with_verification_interval(proofs_history_verification_interval)
                .build()
                .run()
                .boxed())
        })
        .extend_rpc_modules(move |ctx| {
            let api_ext = EthApiExt::new(ctx.registry.eth_api().clone(), storage.clone());
            let debug_ext = DebugApiExt::new(
                ctx.node().provider().clone(),
                ctx.registry.eth_api().clone(),
                storage,
                ctx.node().task_executor().clone(),
                ctx.node().evm_config().clone(),
            );
            ctx.modules.replace_configured(api_ext.into_rpc())?;
            ctx.modules.replace_configured(debug_ext.into_rpc())?;
            Ok(())
        })
}

/// Spawns a task that periodically reports metrics for the proofs DB.
fn spawn_proofs_db_metrics<S>(
    executor: TaskExecutor,
    storage: Arc<S>,
    metrics_report_interval: Duration,
) where
    S: DatabaseMetrics + Send + Sync + 'static,
{
    executor.spawn_critical_task("base-proofs-storage-metrics", async move {
        info!(
            target: "reth::cli",
            ?metrics_report_interval,
            "Starting Base proofs storage metrics task"
        );

        loop {
            sleep(metrics_report_interval).await;
            storage.report_metrics();
        }
    });
}
