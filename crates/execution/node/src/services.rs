//! Fixed background services for Base execution nodes.

use base_execution_state_indexer::ShadowWrite;
use base_execution_state_indexer::{
    ShadowIndexerConfig, ShadowIndexerExEx, ShadowRetention, ShadowWriter,
};
use base_execution_state_provider::CanonStateSubscriptions;
use base_execution_txpool::TransactionValidity;
use base_node_context::BaseNodeContext;
use base_tx_forwarding::{TxForwardingConfig, TxForwardingService};
use base_txpool_tracing::{TxpoolConfig, tracex_subscription};
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;

use crate::{
    BaseExecutionService, ExecutionUpgradeSignalConfig, ExecutionUpgradeSignalRuntime, FullNode,
    ProofHistory, RollupArgs, RpcContext,
};

/// Runtime settings for the services shipped with the Base node.
#[derive(Debug, Default)]
pub struct NodeServices {
    /// Canonical shadow indexing settings.
    pub shadow_indexer: Option<ShadowIndexerConfig>,
    /// Transaction event tracing settings.
    pub tracing: Option<TxpoolConfig>,
    /// Forwarding to configured block builders.
    pub forwarding: Option<TxForwardingConfig>,
    /// Contract-backed upgrade monitoring settings.
    pub upgrade_signal: Option<ExecutionUpgradeSignalConfig>,
}

/// Prepared shadow writer and canonical processing queue.
#[derive(Debug)]
pub struct ShadowIndexerRuntime {
    /// Database and retention settings.
    pub config: ShadowIndexerConfig,
    /// Canonical indexing input.
    pub sender: mpsc::Sender<ShadowWrite>,
    /// Queue consumed by the database writer.
    pub receiver: mpsc::Receiver<ShadowWrite>,
}

/// Resources shared across startup, RPC, and canonical processing.
#[derive(Debug)]
pub struct PreparedNodeServices {
    /// Open proof storage.
    pub proofs: Option<ProofHistory>,
    /// Shadow database writer resources.
    pub shadow_indexer: Option<ShadowIndexerRuntime>,
    /// Transaction event subscription settings.
    pub tracing: Option<TxpoolConfig>,
    /// Transaction forwarding settings.
    pub forwarding: Option<TxForwardingConfig>,
    /// Shared runtime upgrade observer.
    pub upgrade_signal: Option<ExecutionUpgradeSignalRuntime>,
}

impl NodeServices {
    /// Prepares the services before starting node tasks.
    pub fn prepare(self, args: &RollupArgs) -> eyre::Result<PreparedNodeServices> {
        let proofs = args.proofs_history.then(|| ProofHistory::open(args)).transpose()?;
        let shadow_indexer = self.shadow_indexer.filter(|config| config.enabled).map(|config| {
            let (sender, receiver) = mpsc::channel(1024);
            ShadowIndexerRuntime { config, sender, receiver }
        });
        Ok(PreparedNodeServices {
            proofs,
            shadow_indexer,
            tracing: self.tracing,
            forwarding: self.forwarding,
            upgrade_signal: self.upgrade_signal.map(ExecutionUpgradeSignalRuntime::new),
        })
    }
}

impl PreparedNodeServices {
    /// Constructs only the canonical processors supported by Base.
    pub fn execution_services(&self) -> Vec<BaseExecutionService> {
        let mut services = Vec::new();
        if let Some(proofs) = &self.proofs {
            services.push(BaseExecutionService::Proofs(proofs.clone()));
        }
        if let Some(shadow) = &self.shadow_indexer {
            services
                .push(BaseExecutionService::Shadow(ShadowIndexerExEx::new(shadow.sender.clone())));
        }
        services
    }

    /// Subscribes to transaction events before the RPC servers accept transactions.
    pub fn start_tracing(&mut self, node: &BaseNodeContext) {
        if let Some(config) = self.tracing.take().filter(|config| config.tracing_enabled) {
            let canonical = BroadcastStream::new(node.provider.subscribe_to_canonical_state());
            let pool = node.transaction_pool.clone();
            node.task_executor.spawn_task(async move {
                tracex_subscription(
                    canonical,
                    pool,
                    config.tracing_logs_enabled,
                    config.transaction_event_node_role,
                )
                .await;
            });
        }
    }

    /// Installs the fixed proof and upgrade methods before RPC starts.
    pub fn register_rpc(&self, ctx: &mut RpcContext<'_>) -> eyre::Result<()> {
        if let Some(proofs) = &self.proofs {
            proofs.register_rpc(ctx)?;
        }
        if let Some(upgrade) = &self.upgrade_signal {
            upgrade.register_rpc(ctx)?;
        }
        Ok(())
    }

    /// Starts the node's writers, forwarding, and upgrade monitoring.
    pub fn start(self, node: &FullNode) -> eyre::Result<()> {
        if let Some(proofs) = self.proofs {
            proofs.start(node)?;
        }
        if let Some(shadow) = self.shadow_indexer {
            ShadowRetention::spawn(
                &node.task_executor,
                shadow.config.db.clone(),
                shadow.config.retention,
            );
            ShadowWriter::spawn(
                node.task_executor.clone(),
                shadow.receiver,
                shadow.config.db,
                shadow.config.builder_version,
            );
        }
        if let Some(config) =
            self.forwarding.filter(|config| config.enabled && !config.builder_urls.is_empty())
        {
            info!(builder_urls = ?config.builder_urls, resend_after_ms = config.resend_after_ms, max_batch_size = config.max_batch_size, max_rps = config.max_rps, "starting transaction forwarding pipeline");
            let handle = TxForwardingService::new(config)
                .spawn_with_extensions::<_, TransactionValidity>(
                    node.pool.clone(),
                    &node.task_executor,
                );
            node.task_executor.spawn_with_graceful_shutdown_signal(|signal| {
                Box::pin(async move {
                    let _guard = signal.await;
                    let report = handle.shutdown().await;
                    info!(?report, "transaction forwarding pipeline stopped");
                })
            });
        }
        if let Some(upgrade) = self.upgrade_signal {
            upgrade.start(node.clone())?;
        }
        Ok(())
    }
}
