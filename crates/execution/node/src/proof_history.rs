//! Proof history storage, RPCs, and canonical block processing.

use std::{sync::Arc, time::Duration};

use base_execution_exex::BaseProofsExEx;
use base_execution_rpc_handlers::{DebugApiExt, DebugApiOverrideServer, EthApiExt, EthApiOverrideServer};
use base_execution_state_database::DatabaseMetrics;
use base_execution_state_tasks::{
    BaseProofsBatchStore, BaseProofsStorage, MdbxProofsStorage, ProofsProgress,
    RocksdbProofsStorage,
};
use futures::future::BoxFuture;
use reth_exex::ExExContext;

use crate::{FullNode, ProofsHistoryDbBackend, RollupArgs, RpcContext};

/// Proof-history storage supported by the Base node.
#[derive(Debug, Clone)]
pub enum ProofHistoryBackend {
    /// MDBX proof snapshots.
    Mdbx(Arc<MdbxProofsStorage>),
    /// RocksDB proof snapshots.
    Rocksdb(Arc<RocksdbProofsStorage>),
}

/// Prepared proof history shared by RPC and canonical processing.
#[derive(Debug, Clone)]
pub struct ProofHistory {
    /// Open proof storage.
    pub backend: ProofHistoryBackend,
    /// Number of canonical blocks retained.
    pub window: u64,
    /// Delay between pruning passes.
    pub prune_interval: Duration,
    /// Interval for verifying persisted proofs.
    pub verification_interval: u64,
}

impl ProofHistory {
    /// Opens the configured proof database before node launch.
    pub fn open(args: &RollupArgs) -> eyre::Result<Self> {
        let path = args.proofs_history_storage_path.as_ref().ok_or_else(|| {
            eyre::eyre!("--proofs-history requires --proofs-history.storage-path")
        })?;
        args.proofs_history_db.ensure_storage_path_matches(path)?;
        let backend = match args.proofs_history_db {
            ProofsHistoryDbBackend::Mdbx => {
                ProofHistoryBackend::Mdbx(Arc::new(MdbxProofsStorage::new_with_options(
                    path,
                    args.proofs_history_mdbx.storage_options(),
                )?))
            }
            ProofsHistoryDbBackend::Rocksdb => {
                ProofHistoryBackend::Rocksdb(Arc::new(RocksdbProofsStorage::new_with_options(
                    path,
                    args.proofs_history_rocksdb.storage_options()?,
                )?))
            }
        };
        Ok(Self {
            backend,
            window: args.proofs_history_window,
            prune_interval: args.proofs_history_prune_interval,
            verification_interval: args.proofs_history_verification_interval,
        })
    }

    /// Adds proof-history methods to the node's built-in APIs.
    pub fn register_rpc(&self, ctx: &mut RpcContext<'_>) -> eyre::Result<()> {
        match &self.backend {
            ProofHistoryBackend::Mdbx(storage) => Self::register_backend(storage.clone(), ctx),
            ProofHistoryBackend::Rocksdb(storage) => Self::register_backend(storage.clone(), ctx),
        }
    }

    /// Registers the proof handlers with their selected storage backend.
    pub fn register_backend<S: BaseProofsBatchStore + DatabaseMetrics + Send + Sync + 'static>(
        backend: Arc<S>,
        ctx: &mut RpcContext<'_>,
    ) -> eyre::Result<()> {
        let storage: BaseProofsStorage<Arc<S>> = backend.into();
        ctx.modules.replace_configured(
            EthApiExt::new(ctx.registry.eth_api().clone(), storage.clone()).into_rpc(),
        )?;
        ctx.modules.replace_configured(
            DebugApiExt::new(
                ctx.provider().clone(),
                ctx.registry.eth_api().clone(),
                storage,
                ctx.node().task_executor().clone(),
                ctx.node().evm_config().clone(),
            )
            .into_rpc(),
        )?;
        Ok(())
    }

    /// Exposes proof progress and starts storage metrics.
    pub fn start(&self, node: &FullNode) -> eyre::Result<()> {
        match &self.backend {
            ProofHistoryBackend::Mdbx(storage) => Self::start_backend(storage.clone(), node),
            ProofHistoryBackend::Rocksdb(storage) => Self::start_backend(storage.clone(), node),
        }
    }

    /// Starts the shared backend's metrics and progress tracking.
    pub fn start_backend<S: BaseProofsBatchStore + DatabaseMetrics + Send + Sync + 'static>(
        storage: Arc<S>,
        node: &FullNode,
    ) -> eyre::Result<()> {
        node.proofs_progress
            .set(ProofsProgress::new(storage.clone()))
            .map_err(|_| eyre::eyre!("proofs history progress already registered"))?;
        let interval = node.config.metrics.push_gateway_interval;
        node.task_executor.spawn_critical_task("base-proofs-storage-metrics", async move {
            loop {
                tokio::time::sleep(interval).await;
                storage.report_metrics();
            }
        });
        Ok(())
    }

    /// Runs canonical proof processing with the selected backend.
    pub fn run(self, ctx: ExExContext) -> BoxFuture<'static, eyre::Result<()>> {
        match self.backend {
            ProofHistoryBackend::Mdbx(storage) => Box::pin(
                BaseProofsExEx::builder(ctx, storage.into())
                    .with_proofs_history_prune_interval(self.prune_interval)
                    .with_proofs_history_window(self.window)
                    .with_verification_interval(self.verification_interval)
                    .build()
                    .run(),
            ),
            ProofHistoryBackend::Rocksdb(storage) => Box::pin(
                BaseProofsExEx::builder(ctx, storage.into())
                    .with_proofs_history_prune_interval(self.prune_interval)
                    .with_proofs_history_window(self.window)
                    .with_verification_interval(self.verification_interval)
                    .build()
                    .run(),
            ),
        }
    }
}
