//! Pool component for the node builder.

use base_common_consensus::BaseBlock;
use base_execution_txpool::{DiskFileBlobStore, PoolConfig, TransactionPool};
use reth_chain_state::CanonStateSubscriptions;

use crate::BuilderContext;

/// Opens the node's configured blob cache.
pub fn create_blob_store(ctx: &BuilderContext) -> eyre::Result<DiskFileBlobStore> {
    let config = base_execution_txpool::DiskFileBlobStoreConfig::default()
        .with_max_cached_entries(ctx.config().txpool.max_cached_entries);
    Ok(DiskFileBlobStore::open(ctx.config().datadir().blobstore(), config)?)
}

/// Spawn local transaction backup task if enabled.
fn spawn_local_backup_task<Pool>(ctx: &BuilderContext, pool: Pool) -> eyre::Result<()>
where
    Pool: TransactionPool + Clone + 'static,
{
    if !ctx.config().txpool.disable_transactions_backup {
        let data_dir = ctx.config().datadir();
        let transactions_path = ctx
            .config()
            .txpool
            .transactions_backup_path
            .clone()
            .unwrap_or_else(|| data_dir.txpool_transactions());

        let transactions_backup_config =
            base_execution_txpool::LocalTransactionBackupConfig::with_local_txs_backup(
                transactions_path,
            );

        ctx.task_executor().spawn_critical_with_graceful_shutdown_signal(
            "local transactions backup task",
            |shutdown| {
                base_execution_txpool::backup_local_transactions_task(
                    shutdown,
                    pool,
                    transactions_backup_config,
                )
            },
        );
    }
    Ok(())
}

/// Spawn the main maintenance task for transaction pool.
fn spawn_pool_maintenance_task<Pool>(
    ctx: &BuilderContext,
    pool: Pool,
    pool_config: &PoolConfig,
) -> eyre::Result<()>
where
    Pool: base_execution_txpool::TransactionPoolExt<Block = BaseBlock> + Clone + 'static,
{
    let chain_events = ctx.provider().canonical_state_stream();
    let client = ctx.provider().clone();

    ctx.task_executor().spawn_critical_task(
        "txpool maintenance task",
        base_execution_txpool::maintain_transaction_pool_future(
            client,
            pool,
            chain_events,
            ctx.task_executor().clone(),
            base_execution_txpool::MaintainPoolConfig {
                max_tx_lifetime: pool_config.max_queued_lifetime,
                no_local_exemptions: pool_config.local_transactions_config.no_exemptions,
                ..Default::default()
            },
        ),
    );

    Ok(())
}

/// Spawn all maintenance tasks for a transaction pool (backup + main maintenance).
pub fn spawn_maintenance_tasks<Pool>(
    ctx: &BuilderContext,
    pool: Pool,
    pool_config: &PoolConfig,
) -> eyre::Result<()>
where
    Pool: base_execution_txpool::TransactionPoolExt<Block = BaseBlock> + Clone + 'static,
{
    spawn_local_backup_task(ctx, pool.clone())?;
    spawn_pool_maintenance_task(ctx, pool, pool_config)?;
    Ok(())
}
