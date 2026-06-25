//! Command that prunes the Base Proofs storage.

use std::{path::PathBuf, sync::Arc};

use base_common_consensus::BasePrimitives;
use base_execution_chainspec::BaseChainSpec;
use base_execution_trie::{
    BaseProofStoragePruner, BaseProofsStorage, BaseProofsStore, MdbxProofsStorage,
    RocksdbProofsStorage,
};
use base_node_core::{
    DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS, ProofsHistoryDbBackend, ProofsHistoryRocksdbArgs,
    TWELVE_HOURS_IN_BLOCKS,
};
use clap::Parser;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_node_core::version::version_metadata;
use tracing::info;

/// Prunes the proofs storage by removing old proof history and state updates.
#[derive(Debug, Parser)]
pub struct PruneCommand<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// The path to the storage DB for proofs history.
    #[arg(
        long = "proofs-history.storage-path",
        value_name = "PROOFS_HISTORY_STORAGE_PATH",
        required = true
    )]
    pub storage_path: PathBuf,

    /// The on-disk database backend for proofs history.
    #[arg(long = "proofs-history.db", value_name = "PROOFS_HISTORY_DB", default_value = "mdbx")]
    pub proofs_history_db: ProofsHistoryDbBackend,

    /// Runtime tuning options for the `RocksDB` proofs history backend.
    #[command(flatten)]
    pub proofs_history_rocksdb: ProofsHistoryRocksdbArgs,

    /// The window to span blocks for proofs history. Value is the number of blocks.
    /// Default is 1 month of blocks based on 2 seconds block time.
    /// 30 * 24 * 60 * 60 / 2 = `1_296_000`
    ///
    /// Must be greater than 12 hours of blocks based on 2 seconds block time.
    #[arg(
        long = "proofs-history.window",
        default_value_t = DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS,
        value_name = "PROOFS_HISTORY_WINDOW",
        value_parser = clap::value_parser!(u64).range((TWELVE_HOURS_IN_BLOCKS + 1)..)
    )]
    pub proofs_history_window: u64,

    /// The batch size for pruning operations.
    ///
    /// Each batch materializes up to this many blocks of change-set keys before committing, so
    /// reduce this value to bound memory usage when pruning a large range.
    #[arg(
        long = "proofs-history.prune-batch-size",
        default_value_t = 1000,
        value_name = "PROOFS_HISTORY_PRUNE_BATCH_SIZE",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub proofs_history_prune_batch_size: u64,
}

impl<C: ChainSpecParser<ChainSpec = BaseChainSpec>> PruneCommand<C> {
    /// Execute [`PruneCommand`].
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = BasePrimitives>>(
        self,
        runtime: reth_tasks::Runtime,
    ) -> eyre::Result<()> {
        let Self {
            env,
            storage_path,
            proofs_history_db,
            proofs_history_rocksdb,
            proofs_history_window,
            proofs_history_prune_batch_size,
        } = self;

        info!(target: "reth::cli", version = %version_metadata().short_version, "reth starting");
        info!(
            target: "reth::cli",
            path = ?storage_path,
            backend = ?proofs_history_db,
            "Pruning Base proofs storage"
        );
        proofs_history_db.ensure_storage_path_matches(&storage_path)?;

        // Initialize the environment with read-only access
        let Environment { provider_factory, .. } = env.init::<N>(AccessRights::RO, runtime)?;

        match proofs_history_db {
            ProofsHistoryDbBackend::Rocksdb => {
                let storage: BaseProofsStorage<Arc<RocksdbProofsStorage>> = Arc::new(
                    RocksdbProofsStorage::new_with_options(
                        &storage_path,
                        proofs_history_rocksdb.storage_options()?,
                    )
                    .map_err(|e| eyre::eyre!("Failed to create RocksdbProofsStorage: {e}"))?,
                )
                .into();
                Self::prune_storage(
                    storage,
                    provider_factory,
                    proofs_history_window,
                    proofs_history_prune_batch_size,
                )?;
            }
            ProofsHistoryDbBackend::Mdbx => {
                let storage: BaseProofsStorage<Arc<MdbxProofsStorage>> = Arc::new(
                    MdbxProofsStorage::new(&storage_path)
                        .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
                )
                .into();
                Self::prune_storage(
                    storage,
                    provider_factory,
                    proofs_history_window,
                    proofs_history_prune_batch_size,
                )?;
            }
        }

        Ok(())
    }
    fn prune_storage<S, H>(
        storage: BaseProofsStorage<Arc<S>>,
        hash_reader: H,
        proofs_history_window: u64,
        proofs_history_prune_batch_size: u64,
    ) -> eyre::Result<()>
    where
        S: BaseProofsStore + 'static,
        H: reth_provider::BlockHashReader,
    {
        let earliest_block = storage.get_earliest_block_number()?;
        let latest_block = storage.get_latest_block_number()?;
        info!(
            target: "reth::cli",
            ?earliest_block,
            ?latest_block,
            "Current proofs storage block range"
        );

        let pruner = BaseProofStoragePruner::new(
            storage,
            hash_reader,
            proofs_history_window,
            proofs_history_prune_batch_size,
        );
        pruner.run();
        Ok(())
    }
}

impl<C: ChainSpecParser> PruneCommand<C> {
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.env.chain)
    }
}
