use std::{sync::Arc, time::Duration};

use base_client_node::{BaseBuilder, BaseNodeExtension, FromExtensionConfig};
use reth_db::database_metrics::DatabaseMetrics;
use reth_node_api::FullNodeComponents;
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_node::args::RollupArgs;
use reth_optimism_rpc::{
    debug::{DebugApiExt, DebugApiOverrideServer},
    eth::proofs::{EthApiExt, EthApiOverrideServer},
};
use reth_optimism_trie::{MdbxProofsStorage, OpProofsStorage};
use reth_tasks::TaskExecutor;
use tokio::time::sleep;
use tracing::{error, info};

/// Type alias for the proofs history configuration.
pub type ProofsHistoryConfig = RollupArgs;

/// Helper struct that wires the transaction pool features into the node builder.
#[derive(Debug, Clone)]
pub struct ProofsHistoryExtension {
    /// Transaction pool configuration.
    config: RollupArgs,
}

impl ProofsHistoryExtension {
    /// Creates a new transaction pool extension helper.
    pub const fn new(config: RollupArgs) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for ProofsHistoryExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, mut builder: BaseBuilder) -> BaseBuilder {
        // TODO: if BaseBuilder exposes the underlying OpBuilder, we can call launch_node_with_proof_history
        let args = self.config;
        let proofs_history_enabled = args.proofs_history;
        let proofs_history_window = args.proofs_history_window;
        let proofs_history_prune_interval = args.proofs_history_prune_interval;
        let proofs_history_verification_interval = args.proofs_history_verification_interval;

        if proofs_history_enabled {
            let path = args
                .proofs_history_storage_path
                .expect("Path must be provided if not using in-memory storage");
            info!(target: "reth::cli", "Using on-disk storage for proofs history");

            let mdbx = match MdbxProofsStorage::new(&path)
                .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))
            {
                Ok(mdbx) => mdbx,
                Err(e) => {
                    error!(target: "reth::cli", "Failed to create MdbxProofsStorage: {:?}, continuing without proofs history", e);
                    return builder;
                }
            };
            let mdbx = Arc::new(mdbx);
            let storage: OpProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&mdbx).into();

            let storage_exec = storage.clone();

            // ignore unused if metrics feature is disabled
            builder = builder.add_node_started_hook(move |node| {
                spawn_proofs_db_metrics(
                    node.task_executor,
                    mdbx,
                    node.config.metrics.push_gateway_interval,
                );
                Ok(())
            });

            builder = builder
                .install_exex("proofs-history", async move |exex_context| {
                    Ok(OpProofsExEx::builder(exex_context, storage_exec)
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
                        Box::new(ctx.node().task_executor().clone()),
                        ctx.node().evm_config().clone(),
                    );
                    ctx.modules.replace_configured(api_ext.into_rpc())?;
                    ctx.modules.replace_configured(debug_ext.into_rpc())?;
                    Ok(())
                });
        }
        builder
    }
}

impl FromExtensionConfig for ProofsHistoryExtension {
    type Config = ProofsHistoryConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

/// Spawns a task that periodically reports metrics for the proofs DB.
fn spawn_proofs_db_metrics(
    executor: TaskExecutor,
    storage: Arc<MdbxProofsStorage>,
    metrics_report_interval: Duration,
) {
    executor.spawn_critical("op-proofs-storage-metrics", async move {
        info!(
            target: "reth::cli",
            ?metrics_report_interval,
            "Starting op-proofs-storage metrics task"
        );

        loop {
            sleep(metrics_report_interval).await;
            storage.report_metrics();
        }
    });
}
