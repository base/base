//! Command that initializes the Base Proofs storage with the current state of the chain.

use std::{path::PathBuf, sync::Arc};

use base_common_consensus::BasePrimitives;
use base_execution_chainspec::BaseChainSpec;
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, BaseProofsStore, InitializationJob,
    RethTrieStorageLayout, RocksdbProofsStorage,
};
use base_node_core::args::{DeprecatedProofsHistoryDbArgs, ProofsHistoryRocksdbArgs};
use clap::Parser;
use reth_chainspec::ChainInfo;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_node_core::version::version_metadata;
use reth_provider::{BlockNumReader, DBProvider, DatabaseProviderFactory, StorageSettingsCache};
use tracing::info;

/// Initializes the proofs storage with the current state of the chain.
///
/// This command must be run before starting the node with proofs history enabled.
/// It backfills the proofs storage with trie nodes from the current chain state.
#[derive(Debug, Parser)]
pub struct InitCommand<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// The path to the storage DB for proofs history.
    ///
    /// This should match the path used when starting the node with
    /// `--proofs-history.storage-path`.
    #[arg(
        long = "proofs-history.storage-path",
        visible_alias = "proofs.storage-path",
        value_name = "PROOFS_HISTORY_STORAGE_PATH",
        required = true
    )]
    pub storage_path: PathBuf,

    /// Deprecated proofs history database selection flags.
    #[command(flatten)]
    pub deprecated_proofs_history_db: DeprecatedProofsHistoryDbArgs,

    /// Runtime tuning options for the `RocksDB` proofs history backend.
    #[command(flatten)]
    pub proofs_history_rocksdb: ProofsHistoryRocksdbArgs,
}

impl<C: ChainSpecParser<ChainSpec = BaseChainSpec>> InitCommand<C> {
    /// Execute the `proofs init` command.
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = BasePrimitives>>(
        self,
        runtime: reth_tasks::Runtime,
    ) -> eyre::Result<()> {
        let Self { env, storage_path, deprecated_proofs_history_db, proofs_history_rocksdb } = self;

        info!(target: "reth::cli", version = %version_metadata().short_version, "reth starting");
        info!(
            target: "reth::cli",
            path = ?storage_path,
            "Initializing Base proofs storage"
        );
        deprecated_proofs_history_db.warn_if_set();
        base_node_core::args::ensure_rocksdb_storage_path(&storage_path)?;

        // Initialize the environment with read-only access
        let Environment { provider_factory, .. } = env.init::<N>(AccessRights::RO, runtime)?;

        let storage: BaseProofsStorage<Arc<RocksdbProofsStorage>> = Arc::new(
            RocksdbProofsStorage::new_with_options(
                &storage_path,
                proofs_history_rocksdb.storage_options()?,
            )
            .map_err(|e| eyre::eyre!("Failed to create RocksdbProofsStorage: {e}"))?,
        )
        .into();
        Self::initialize_storage(storage, &provider_factory)?;

        Ok(())
    }

    fn initialize_storage<S, F>(
        storage: BaseProofsStorage<Arc<S>>,
        provider_factory: &F,
    ) -> eyre::Result<()>
    where
        S: BaseProofsInitialStateStore + BaseProofsStore + 'static,
        F: BlockNumReader + DatabaseProviderFactory + StorageSettingsCache,
    {
        // Check if already initialized
        if let Some((block_number, block_hash)) = storage.get_earliest_block_number()? {
            info!(
                target: "reth::cli",
                block_number = block_number,
                block_hash = ?block_hash,
                "Proofs storage already initialized"
            );
            return Ok(());
        }

        // Get the current chain state
        let ChainInfo { best_number, best_hash, .. } = provider_factory.chain_info()?;

        info!(
            target: "reth::cli",
            best_number = best_number,
            best_hash = ?best_hash,
            "Starting backfill job for current chain state"
        );

        // Run the backfill job
        {
            let trie_layout = if provider_factory.cached_storage_settings().is_v2() {
                RethTrieStorageLayout::Packed
            } else {
                RethTrieStorageLayout::Legacy
            };
            let db_provider =
                provider_factory.database_provider_ro()?.disable_long_read_transaction_safety();
            let db_tx = db_provider.into_tx();

            InitializationJob::new(storage, db_tx, trie_layout).run(best_number, best_hash)?;
        }

        info!(
            target: "reth::cli",
            best_number = best_number,
            best_hash = ?best_hash,
            "Proofs storage initialized successfully"
        );

        Ok(())
    }
}

impl<C: ChainSpecParser> InitCommand<C> {
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.env.chain)
    }
}
