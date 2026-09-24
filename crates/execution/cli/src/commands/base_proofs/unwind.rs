//! Command that unwinds the Base Proofs storage to a specific block number.

use std::{path::PathBuf, sync::Arc};

use base_common_consensus::BasePrimitives;
use base_execution_chainspec::BaseChainSpec;
use base_execution_trie::{BaseProofsStorage, BaseProofsStore, RocksdbProofsStorage};
use base_node_core::args::ProofsHistoryRocksdbArgs;
use clap::Parser;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_node_core::{primitives::AlloyBlockHeader as _, version::version_metadata};
use reth_provider::{BlockReader, TransactionVariant};
use tracing::{info, warn};

/// Unwinds the proofs storage to a specific block number.
///
/// This command removes all proof history and state updates after the target block number.
#[derive(Debug, Parser)]
pub struct UnwindCommand<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// The path to the storage DB for proofs history.
    #[arg(
        long = "proofs-history.storage-path",
        visible_alias = "proofs.storage-path",
        value_name = "PROOFS_HISTORY_STORAGE_PATH",
        required = true
    )]
    pub storage_path: PathBuf,

    /// Runtime tuning options for the `RocksDB` proofs history backend.
    #[command(flatten)]
    pub proofs_history_rocksdb: ProofsHistoryRocksdbArgs,

    /// The target block number to unwind to.
    ///
    /// All history *at and after* this block will be removed.
    #[arg(long, value_name = "TARGET_BLOCK")]
    pub target: u64,
}

impl<C: ChainSpecParser<ChainSpec = BaseChainSpec>> UnwindCommand<C> {
    /// Execute [`UnwindCommand`].
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = BasePrimitives>>(
        self,
        runtime: reth_tasks::Runtime,
    ) -> eyre::Result<()> {
        let Self { env, storage_path, proofs_history_rocksdb, target } = self;

        info!(target: "reth::cli", version = %version_metadata().short_version, "reth starting");
        info!(
            target: "reth::cli",
            path = ?storage_path,
            "Unwinding Base proofs storage"
        );
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
        Self::unwind_storage(target, &provider_factory, storage)?;

        Ok(())
    }

    fn unwind_storage<S, P>(
        target: u64,
        provider_factory: &P,
        storage: BaseProofsStorage<Arc<S>>,
    ) -> eyre::Result<()>
    where
        S: BaseProofsStore + 'static,
        P: BlockReader,
    {
        // Validate that the target block is within a valid range for unwinding
        if !Self::validate_unwind_range(target, &storage)? {
            return Ok(());
        }

        // Get the target block from the main database
        let block = provider_factory
            .recovered_block(target.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| eyre::eyre!("Target block {} not found in the main database", target))?;

        info!(
            target: "reth::cli",
            block_number = block.number(),
            block_hash = %block.hash(),
            "Unwinding to target block"
        );
        storage.unwind_history(block.block_with_parent())?;
        Ok(())
    }

    /// Validates that the target block number is within a valid range for unwinding.
    fn validate_unwind_range<Store: BaseProofsStore>(
        target: u64,
        storage: &BaseProofsStorage<Store>,
    ) -> eyre::Result<bool> {
        let (Some((earliest, _)), Some((latest, _))) =
            (storage.get_earliest_block_number()?, storage.get_latest_block_number()?)
        else {
            warn!(target: "reth::cli", "No blocks found in proofs storage. Nothing to unwind.");
            return Ok(false);
        };

        if target <= earliest {
            warn!(target: "reth::cli", unwind_target = ?target, ?earliest, "Target block is less than the earliest block in proofs storage. Nothing to unwind.");
            return Ok(false);
        }

        if target > latest {
            warn!(target: "reth::cli", unwind_target = ?target, ?latest, "Target block is not less than the latest block in proofs storage. Nothing to unwind.");
            return Ok(false);
        }

        Ok(true)
    }
}

impl<C: ChainSpecParser> UnwindCommand<C> {
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.env.chain)
    }
}
