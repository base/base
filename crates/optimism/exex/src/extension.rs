use clap::{builder::ArgPredicate};
use base_client_node::{BaseNodeExtension, FromExtensionConfig, OpBuilder};
use reth_db::database_metrics::DatabaseMetrics;
use reth_optimism_trie::{MdbxProofsStorage, OpProofsStorage};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info};
use std::sync::Arc;
use crate::OpProofsExEx;
use reth_optimism_rpc::{
    debug::{DebugApiExt, DebugApiOverrideServer},
    eth::proofs::{EthApiExt, EthApiOverrideServer},
};
use reth_tasks::TaskExecutor;
use tokio::time::sleep;
use reth_node_api::FullNodeComponents;
// use reth_rpc_eth_api::node::RpcNodeCore;

/// Transaction pool configuration.
#[derive(Debug, Clone, clap::Args, Eq, PartialEq)]
pub struct ProofsHistoryConfig {

    /// If true, initialize external-proofs exex to save and serve trie nodes to provide proofs
    /// faster.
    #[arg(
        long = "proofs-history",
        value_name = "PROOFS_HISTORY",
        default_value_ifs([
            ("proofs-history.storage-path", ArgPredicate::IsPresent, "true")
        ])
    )]
    pub proofs_history: bool,

    /// The path to the storage DB for proofs history.
    #[arg(long = "proofs-history.storage-path", value_name = "PROOFS_HISTORY_STORAGE_PATH")]
    pub proofs_history_storage_path: Option<PathBuf>,

    /// The window to span blocks for proofs history. Value is the number of blocks.
    /// Default is 1 month of blocks based on 2 seconds block time.
    /// 30 * 24 * 60 * 60 / 2 = `1_296_000`
    #[arg(
        long = "proofs-history.window",
        default_value_t = 1_296_000,
        value_name = "PROOFS_HISTORY_WINDOW"
    )]
    pub proofs_history_window: u64,

    /// Interval between proof-storage prune runs. Accepts human-friendly durations
    /// like "100s", "5m", "1h". Defaults to 15s.
    ///
    /// - Shorter intervals prune smaller batches more often, so each prune run tends to be faster
    ///   and the blocking pause for writes is shorter, at the cost of more frequent pauses.
    /// - Longer intervals prune larger batches less often, which reduces how often pruning runs,
    ///   but each run can take longer and block writes for longer.
    ///
    /// A shorter interval is preferred so that prune
    /// runs stay small and don’t stall writes for too long.
    ///
    /// CLI: `--proofs-history.prune-interval 10m`
    #[arg(
        long = "proofs-history.prune-interval",
        value_name = "PROOFS_HISTORY_PRUNE_INTERVAL",
        default_value = "15s",
        value_parser = humantime::parse_duration
    )]
    pub proofs_history_prune_interval: Duration,
}

/// Helper struct that wires the transaction pool features into the node builder.
#[derive(Debug, Clone)]
pub struct ProofsHistoryExtension {
    /// Transaction pool configuration.
    config: ProofsHistoryConfig,
}

impl ProofsHistoryExtension {
    /// Creates a new transaction pool extension helper.
    pub const fn new(config: ProofsHistoryConfig) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for ProofsHistoryExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, mut builder: OpBuilder) -> OpBuilder {
        let args = self.config;
        let proofs_history_enabled = args.proofs_history;
        let proofs_history_window = args.proofs_history_window;
        let proofs_history_prune_interval = args.proofs_history_prune_interval;
        
        if proofs_history_enabled {
            let path = args
                .proofs_history_storage_path
                .clone()
                .expect("Path must be provided if not using in-memory storage");
            info!(target: "reth::cli", "Using on-disk storage for proofs history");
    

            let mdbx = match MdbxProofsStorage::new(&path).map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}")) {
                Ok(mdbx) => mdbx,
                Err(e) => {
                    error!(target: "reth::cli", "Failed to create MdbxProofsStorage: {:?}, continuing without proofs history", e);
                    return builder;
                }
            };
            let mdbx = Arc::new(
                mdbx,
            );
            let storage: OpProofsStorage<_> = mdbx.clone().into();
    
            let storage_exec = storage.clone();
    
            builder = builder
                .on_node_started(move |node| {
                    spawn_proofs_db_metrics(
                        node.task_executor,
                        mdbx,
                        node.config.metrics.push_gateway_interval,
                    );
                    Ok(())
                })
                .install_exex("proofs-history", async move |exex_context| {
                    Ok(OpProofsExEx::new(
                        exex_context,
                        storage_exec,
                        proofs_history_window,
                        proofs_history_prune_interval,
                    )
                    .run())
                })
                .extend_rpc_modules(move |ctx| {
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