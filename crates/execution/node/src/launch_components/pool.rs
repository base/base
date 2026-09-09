//! Pool component for the node builder.

use alloy_primitives::map::AddressSet;
use base_common_consensus::BaseBlock;
use base_execution_txpool::{
    BaseOrdering, BlobStore, DiskFileBlobStore, PoolConfig, SubPoolLimit, TransactionPool,
    TransactionValidationTaskExecutor, TransactionValidator,
};
use reth_chain_state::CanonStateSubscriptions;

use crate::BuilderContext;

/// Convenience type to override cli or default pool configuration during build.
#[derive(Debug, Clone, Default)]
pub struct PoolBuilderConfigOverrides {
    /// Max number of transactions in the pending sub-pool
    pub pending_limit: Option<SubPoolLimit>,
    /// Max number of transactions in the basefee sub-pool
    pub basefee_limit: Option<SubPoolLimit>,
    /// Max number of transactions in the queued sub-pool
    pub queued_limit: Option<SubPoolLimit>,
    /// Max number of transactions in the blob sub-pool
    pub blob_limit: Option<SubPoolLimit>,
    /// Max number of executable transaction slots guaranteed per account
    pub max_account_slots: Option<usize>,
    /// Minimum base fee required by the protocol.
    pub minimal_protocol_basefee: Option<u64>,
    /// Addresses that will be considered as local. Above exemptions apply.
    pub local_addresses: AddressSet,
    /// Additional tasks to validate new transactions.
    pub additional_validation_tasks: Option<usize>,
}

impl PoolBuilderConfigOverrides {
    /// Applies the configured overrides to the given [`PoolConfig`].
    pub fn apply(self, mut config: PoolConfig) -> PoolConfig {
        let Self {
            pending_limit,
            basefee_limit,
            queued_limit,
            blob_limit,
            max_account_slots,
            minimal_protocol_basefee,
            local_addresses,
            additional_validation_tasks: _,
        } = self;

        if let Some(pending_limit) = pending_limit {
            config.pending_limit = pending_limit;
        }
        if let Some(basefee_limit) = basefee_limit {
            config.basefee_limit = basefee_limit;
        }
        if let Some(queued_limit) = queued_limit {
            config.queued_limit = queued_limit;
        }
        if let Some(blob_limit) = blob_limit {
            config.blob_limit = blob_limit;
        }
        if let Some(max_account_slots) = max_account_slots {
            config.max_account_slots = max_account_slots;
        }
        if let Some(minimal_protocol_basefee) = minimal_protocol_basefee {
            config.minimal_protocol_basefee = minimal_protocol_basefee;
        }
        config.local_transactions_config.local_addresses.extend(local_addresses);

        config
    }
}

/// A builder for creating transaction pools with common configuration options.
///
/// This builder provides a fluent API for setting up transaction pools with various
/// configurations like blob stores, validators, and maintenance tasks.
pub struct TxPoolBuilder<'a, V = ()> {
    ctx: &'a BuilderContext,
    validator: V,
}

impl<'a> TxPoolBuilder<'a> {
    /// Creates a new `TxPoolBuilder` with the given context.
    pub const fn new(ctx: &'a BuilderContext) -> Self {
        Self { ctx, validator: () }
    }
}

impl<'a, V> TxPoolBuilder<'a, V> {
    /// Configure the validator for the transaction pool.
    pub fn with_validator<NewV>(self, validator: NewV) -> TxPoolBuilder<'a, NewV> {
        TxPoolBuilder { ctx: self.ctx, validator }
    }
}

impl<'a, V> TxPoolBuilder<'a, TransactionValidationTaskExecutor<V>>
where
    V: TransactionValidator<Block = BaseBlock> + 'static,
{
    /// Consume the type and build the [`base_execution_txpool::Pool`] with the given config and
    /// blob store.
    pub fn build<BS>(
        self,
        blob_store: BS,
        pool_config: PoolConfig,
    ) -> base_execution_txpool::Pool<TransactionValidationTaskExecutor<V>, BS>
    where
        BS: BlobStore,
    {
        let TxPoolBuilder { validator, .. } = self;
        base_execution_txpool::Pool::new(
            validator,
            BaseOrdering::default(),
            blob_store,
            pool_config,
        )
    }

    /// Build the transaction pool and spawn its maintenance tasks.
    /// This method creates the blob store, builds the pool, and spawns maintenance tasks.
    pub fn build_and_spawn_maintenance_task<BS>(
        self,
        blob_store: BS,
        pool_config: PoolConfig,
    ) -> eyre::Result<base_execution_txpool::Pool<TransactionValidationTaskExecutor<V>, BS>>
    where
        BS: BlobStore + Clone,
    {
        self.build_with_ordering_and_spawn_maintenance_task(
            BaseOrdering::default(),
            blob_store,
            pool_config,
        )
    }

    /// Build the transaction pool with a custom [`BaseOrdering`] and spawn its maintenance
    /// tasks.
    pub fn build_with_ordering_and_spawn_maintenance_task<BS>(
        self,
        ordering: base_execution_txpool::BaseOrdering,
        blob_store: BS,
        pool_config: PoolConfig,
    ) -> eyre::Result<base_execution_txpool::Pool<TransactionValidationTaskExecutor<V>, BS>>
    where
        BS: BlobStore + Clone,
    {
        let TxPoolBuilder { ctx, validator, .. } = self;

        let transaction_pool =
            base_execution_txpool::Pool::new(validator, ordering, blob_store, pool_config.clone());

        spawn_maintenance_tasks(ctx, transaction_pool.clone(), &pool_config)?;

        Ok(transaction_pool)
    }
}

/// Create blob store with default configuration.
pub fn create_blob_store(ctx: &BuilderContext) -> eyre::Result<DiskFileBlobStore> {
    let cache_size = Some(ctx.config().txpool.max_cached_entries);
    create_blob_store_with_cache(ctx, cache_size)
}

/// Create blob store with custom cache size configuration for how many blobs should be cached in
/// memory.
pub fn create_blob_store_with_cache(
    ctx: &BuilderContext,
    cache_size: Option<u32>,
) -> eyre::Result<DiskFileBlobStore> {
    let data_dir = ctx.config().datadir();
    let config = if let Some(cache_size) = cache_size {
        base_execution_txpool::DiskFileBlobStoreConfig::default()
            .with_max_cached_entries(cache_size)
    } else {
        Default::default()
    };

    Ok(base_execution_txpool::DiskFileBlobStore::open(data_dir.blobstore(), config)?)
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

impl<V: std::fmt::Debug> std::fmt::Debug for TxPoolBuilder<'_, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxPoolBuilder").field("validator", &self.validator).finish()
    }
}

#[cfg(test)]
mod tests {
    use base_execution_txpool::PoolConfig;

    use super::*;

    #[test]
    fn test_pool_builder_config_overrides_apply() {
        let base_config = PoolConfig::default();
        let overrides = PoolBuilderConfigOverrides {
            pending_limit: Some(SubPoolLimit::default()),
            max_account_slots: Some(100),
            minimal_protocol_basefee: Some(1000),
            ..Default::default()
        };

        let updated_config = overrides.apply(base_config);
        assert_eq!(updated_config.max_account_slots, 100);
        assert_eq!(updated_config.minimal_protocol_basefee, 1000);
    }

    #[test]
    fn test_pool_builder_config_overrides_default() {
        let overrides = PoolBuilderConfigOverrides::default();
        assert!(overrides.pending_limit.is_none());
        assert!(overrides.max_account_slots.is_none());
        assert!(overrides.local_addresses.is_empty());
    }
}
