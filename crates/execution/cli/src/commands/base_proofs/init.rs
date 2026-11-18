//! Command that initializes the Base Proofs storage with the current state of the chain.

use std::{path::PathBuf, sync::Arc};

use base_common_consensus::BasePrimitives;
use base_execution_chainspec::BaseChainSpec;
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, BaseProofsStore, InitializationJob,
    MdbxProofsStorage, RethTrieStorageLayout, RocksdbProofsStorage,
};
use base_node_core::args::{ProofsHistoryDbBackend, ProofsHistoryRocksdbArgs};
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
/// Run with the execution node stopped; consistency checks may repair execution storage.
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

    /// The on-disk database backend for proofs history.
    #[arg(
        long = "proofs-history.db",
        visible_alias = "proofs.db",
        value_name = "PROOFS_HISTORY_DB",
        default_value = "mdbx"
    )]
    pub proofs_history_db: ProofsHistoryDbBackend,

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
        let Self { env, storage_path, proofs_history_db, proofs_history_rocksdb } = self;

        info!(target: "reth::cli", version = %version_metadata().short_version, "reth starting");
        info!(
            target: "reth::cli",
            path = ?storage_path,
            backend = ?proofs_history_db,
            "Initializing Base proofs storage"
        );
        proofs_history_db.ensure_storage_path_matches(&storage_path)?;

        // Initialize the environment with read-write access so startup consistency checks can
        // heal interrupted static-file commits before proofs backfill reads the chain state.
        // This command runs offline; the backfill transaction itself remains read-only.
        let Environment { provider_factory, .. } = env.init::<N>(AccessRights::RW, runtime)?;

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
                Self::initialize_storage(storage, &provider_factory)?;
            }
            ProofsHistoryDbBackend::Mdbx => {
                let storage: BaseProofsStorage<Arc<MdbxProofsStorage>> = Arc::new(
                    MdbxProofsStorage::new(&storage_path)
                        .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
                )
                .into();
                Self::initialize_storage(storage, &provider_factory)?;
            }
        }

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

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use base_node_core::BaseNode;
    use reth_provider::{StaticFileProviderFactory, StaticFileSegment, StaticFileWriter};
    use reth_tasks::Runtime;
    use tempfile::TempDir;

    use super::*;
    use crate::chainspec::BaseChainSpecParser;

    /// Creates committed static-file data one block ahead of the database checkpoint.
    fn interrupted_static_file_commit(
        directory: &TempDir,
        runtime: Runtime,
    ) -> eyre::Result<InitCommand<BaseChainSpecParser>> {
        let datadir = directory.path().join("execution");
        let proofs = directory.path().join("proofs");
        let command = InitCommand::try_parse_from([
            "init",
            "--chain",
            "base-sepolia",
            "--datadir",
            datadir.to_str().expect("temporary datadir is UTF-8"),
            "--proofs-history.storage-path",
            proofs.to_str().expect("temporary proofs path is UTF-8"),
        ])?;
        {
            let environment = command.env.init::<BaseNode>(AccessRights::RW, runtime)?;
            let static_files = environment.provider_factory.static_file_provider();
            let header = Header { number: 1, ..Default::default() };
            {
                let mut writer = static_files.latest_writer(StaticFileSegment::Headers)?;
                writer.append_header(&header, &header.hash_slow())?;
                writer.commit()?;
            }
            assert_eq!(
                static_files.get_highest_static_file_block(StaticFileSegment::Headers),
                Some(1),
                "fixture must retain the interrupted static-file commit above genesis"
            );
        }
        Ok(command)
    }


    /// Verifies offline proofs initialization heals an interrupted execution static-file commit.
    #[tokio::test]
    async fn proofs_init_heals_static_files_before_backfill() -> eyre::Result<()> {
        let directory = TempDir::new()?;
        let runtime = Runtime::test();
        let command = interrupted_static_file_commit(&directory, runtime.clone())?;
        let datadir = directory.path().join("execution");
        let result = command.execute::<BaseNode>(runtime.clone()).await;
        assert!(result.is_ok(), "offline proofs initialization must heal before backfilling: {result:?}");
        let env = EnvironmentArgs::<BaseChainSpecParser>::try_parse_from([
            "inspect",
            "--chain",
            "base-sepolia",
            "--datadir",
            datadir.to_str().expect("temporary datadir is UTF-8"),
        ])?;
        let environment = env.init::<BaseNode>(AccessRights::RO, runtime)?;
        assert_eq!(
            environment.provider_factory.static_file_provider()
                .get_highest_static_file_block(StaticFileSegment::Headers),
            Some(0),
            "initialization must remove the uncommitted header above the database checkpoint"
        );
        Ok(())
    }
}
