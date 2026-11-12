use std::{sync::Arc, time::Duration};

use base_execution_exex::BaseProofsExEx;
use base_execution_rpc::{
    debug::{DebugApiExt, DebugApiOverrideServer},
    eth::proofs::{EthApiExt, EthApiOverrideServer},
};
use base_execution_trie::{
    BaseProofsBatchStore, BaseProofsStorage, MdbxProofsStorage, RocksdbProofsStorage,
};
use base_node_core::args::{ProofsHistoryDbBackend, RollupArgs};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_db::database_metrics::DatabaseMetrics;
use reth_node_api::FullNodeComponents;
use reth_tasks::TaskExecutor;
use tokio::time::sleep;
use tracing::{error, info};

/// Type alias for the proofs history configuration.
pub type ProofsHistoryConfig = RollupArgs;

/// Helper struct that wires proofs history into the node builder.
#[derive(Debug, Clone)]
pub struct ProofsHistoryExtension {
    /// Proofs history configuration.
    config: RollupArgs,
}

impl ProofsHistoryExtension {
    /// Creates a new proofs history extension helper.
    pub const fn new(config: RollupArgs) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for ProofsHistoryExtension {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, mut hooks: NodeHooks) -> NodeHooks {
        // TODO: if NodeHooks exposes the underlying Builder, we can call launch_node_with_proof_history
        let args = self.config;
        let proofs_history_enabled = args.proofs_history;
        let proofs_history_db = args.proofs_history_db;
        let proofs_history_rocksdb = args.proofs_history_rocksdb;
        let proofs_history_mdbx = args.proofs_history_mdbx;
        let proofs_history_window = args.proofs_history_window;
        let proofs_history_prune_interval = args.proofs_history_prune_interval;
        let proofs_history_verification_interval = args.proofs_history_verification_interval;

        if proofs_history_enabled {
            let Some(path) = args.proofs_history_storage_path else {
                error!(
                    target: "reth::cli",
                    "--proofs-history requires --proofs-history.storage-path"
                );
                return hooks.add_node_started_hook(|_| {
                    Err(eyre::eyre!("--proofs-history requires --proofs-history.storage-path"))
                });
            };
            info!(target: "reth::cli", "Using on-disk storage for proofs history");

            if let Err(e) = proofs_history_db.ensure_storage_path_matches(&path) {
                error!(target: "reth::cli", error = ?e, "Proofs history storage path does not match selected backend");
                return hooks.add_node_started_hook(move |_| Err(e));
            }

            match proofs_history_db {
                ProofsHistoryDbBackend::Rocksdb => {
                    let storage_options = match proofs_history_rocksdb.storage_options() {
                        Ok(options) => options,
                        Err(e) => {
                            error!(target: "reth::cli", error = ?e, "Invalid RocksDB proofs history options");
                            return hooks.add_node_started_hook(move |_| Err(e));
                        }
                    };
                    let rocksdb =
                        match RocksdbProofsStorage::new_with_options(&path, storage_options)
                            .map_err(|e| eyre::eyre!("Failed to create RocksdbProofsStorage: {e}"))
                        {
                            Ok(rocksdb) => rocksdb,
                            Err(e) => {
                                error!(
                                    target: "reth::cli",
                                    error = ?e,
                                    "Failed to create RocksdbProofsStorage"
                                );
                                return hooks.add_node_started_hook(move |_| Err(e));
                            }
                        };
                    hooks = install_proofs_history(
                        hooks,
                        Arc::new(rocksdb),
                        proofs_history_window,
                        proofs_history_prune_interval,
                        proofs_history_verification_interval,
                    );
                }
                ProofsHistoryDbBackend::Mdbx => {
                    let storage_options = proofs_history_mdbx.storage_options();
                    let mdbx = match MdbxProofsStorage::new_with_options(&path, storage_options)
                        .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))
                    {
                        Ok(mdbx) => mdbx,
                        Err(e) => {
                            error!(
                                target: "reth::cli",
                                error = ?e,
                                "Failed to create MdbxProofsStorage"
                            );
                            return hooks.add_node_started_hook(move |_| Err(e));
                        }
                    };
                    hooks = install_proofs_history(
                        hooks,
                        Arc::new(mdbx),
                        proofs_history_window,
                        proofs_history_prune_interval,
                        proofs_history_verification_interval,
                    );
                }
            }
        }
        hooks
    }
}

impl FromExtensionConfig for ProofsHistoryExtension {
    type Config = ProofsHistoryConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

fn install_proofs_history<S>(
    mut hooks: NodeHooks,
    storage_backend: Arc<S>,
    proofs_history_window: u64,
    proofs_history_prune_interval: Duration,
    proofs_history_verification_interval: u64,
) -> NodeHooks
where
    S: BaseProofsBatchStore + DatabaseMetrics + Send + Sync + 'static,
{
    let storage: BaseProofsStorage<Arc<S>> = Arc::clone(&storage_backend).into();
    let storage_exec = storage.clone();

    hooks = hooks.add_node_started_hook(move |node| {
        spawn_proofs_db_metrics(
            node.task_executor,
            storage_backend,
            node.config.metrics.push_gateway_interval,
        );
        Ok(())
    });

    hooks
        .install_exex("proofs-history", async move |exex_context| {
            Ok(BaseProofsExEx::builder(exex_context, storage_exec)
                .with_proofs_history_prune_interval(proofs_history_prune_interval)
                .with_proofs_history_window(proofs_history_window)
                .with_verification_interval(proofs_history_verification_interval)
                .build()
                .run())
        })
        .add_rpc_module(move |ctx| {
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
