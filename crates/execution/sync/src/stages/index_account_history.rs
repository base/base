use std::fmt::Debug;

use base_execution_state_database::{DbTxMut, Tables, tables};
use base_execution_state_provider::{
    DBProvider, EitherWriter, HistoryWriter, PruneCheckpointReader, PruneCheckpointWriter,
    RocksDBProviderFactory, StorageSettingsCache,
};
use base_execution_state_types::{
    EtlConfig, PruneCheckpoint, PruneMode, PrunePurpose, PruneSegment,
};
use tracing::info;

use super::collect_account_history_indices;
use crate::{
    BlockRangeOutput, ExecInput, ExecOutput, IndexHistoryConfig, Stage, StageCheckpoint,
    StageError, StageId, UnwindInput, UnwindOutput,
    stages::utils::{
        load_account_history_append, prepare_account_history_writes, write_prepared_history_shards,
    },
};

/// Stage is indexing history the account changesets generated in
/// [`ExecutionStage`][crate::stages::ExecutionStage]. For more information
/// on index sharding take a look at [`tables::AccountsHistory`]
#[derive(Debug)]
pub struct IndexAccountHistoryStage {
    /// Number of blocks after which the control
    /// flow will be returned to the pipeline for commit.
    pub commit_threshold: u64,
    /// Pruning configuration.
    pub prune_mode: Option<PruneMode>,
    /// ETL configuration
    pub etl_config: EtlConfig,
}

impl IndexAccountHistoryStage {
    /// Create new instance of [`IndexAccountHistoryStage`].
    pub const fn new(
        config: IndexHistoryConfig,
        etl_config: EtlConfig,
        prune_mode: Option<PruneMode>,
    ) -> Self {
        Self { commit_threshold: config.commit_threshold, etl_config, prune_mode }
    }
}

impl Default for IndexAccountHistoryStage {
    fn default() -> Self {
        Self { commit_threshold: 100_000, prune_mode: None, etl_config: EtlConfig::default() }
    }
}

impl<Provider> Stage<Provider> for IndexAccountHistoryStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + HistoryWriter
        + PruneCheckpointReader
        + PruneCheckpointWriter
        + base_execution_state_types::ChangeSetReader
        + base_execution_state_provider::StaticFileProviderFactory
        + StorageSettingsCache
        + RocksDBProviderFactory,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::IndexAccountHistory
    }

    /// Execute the stage.
    fn execute(
        &mut self,
        provider: &Provider,
        mut input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        let initial_sync = input.checkpoint.is_none();
        let rebuild_from_empty = input.checkpoint().block_number == 0;
        let mut prune_target_applied = false;

        if let Some((target_prunable_block, prune_mode)) = self
            .prune_mode
            .map(|mode| {
                mode.prune_target_block(
                    input.target(),
                    PruneSegment::AccountHistory,
                    PrunePurpose::User,
                )
            })
            .transpose()?
            .flatten()
            && (target_prunable_block > input.checkpoint().block_number
                || target_prunable_block == 0 && input.checkpoint().block_number == 0)
        {
            prune_target_applied = true;
            input.checkpoint = Some(StageCheckpoint::new(target_prunable_block));

            // Save prune checkpoint only if we don't have one already.
            // Otherwise, pruner may skip the unpruned range of blocks.
            if provider.get_prune_checkpoint(PruneSegment::AccountHistory)?.is_none() {
                provider.save_prune_checkpoint(
                    PruneSegment::AccountHistory,
                    PruneCheckpoint {
                        block_number: Some(target_prunable_block),
                        tx_number: None,
                        prune_mode,
                    },
                )?;
            }
        }

        if input.target_reached() && prune_target_applied {
            provider.rocksdb_provider().clear::<tables::AccountsHistory>()?;

            return Ok(ExecOutput::done(input.checkpoint()));
        }
        if input.target_reached() && !initial_sync {
            return Ok(ExecOutput::done(input.checkpoint()));
        }

        let BlockRangeOutput { mut block_range, is_final_range } =
            if initial_sync && input.target() == 0 {
                BlockRangeOutput { block_range: 0..=0, is_final_range: true }
            } else {
                input.next_block_range_with_threshold(self.commit_threshold.max(1))
            };

        // A zero checkpoint means no history range is durable. Clear stale rows before using the
        // append-only loader, including when pruning advanced the in-memory checkpoint.
        if rebuild_from_empty {
            // RocksDB clear executes immediately. A crash before commit leaves the durable
            // checkpoint at zero, so the next attempt clears and rebuilds again.
            provider.rocksdb_provider().clear::<tables::AccountsHistory>()?;

            if input.checkpoint().block_number == 0 && !prune_target_applied {
                block_range = 0..=*block_range.end();
            }
        }

        info!(target: "sync::stages::index_account_history::exec", ?rebuild_from_empty, "Collecting indices");

        let collector = {
            // Use the provider-based collection that can read from static files.
            collect_account_history_indices(provider, block_range.clone(), &self.etl_config)?
        };

        info!(target: "sync::stages::index_account_history::exec", "Loading indices into database");

        if rebuild_from_empty {
            provider.with_rocksdb_batch_auto_commit(|rocksdb_batch| {
                let mut writer = EitherWriter::new_accounts_history(provider, rocksdb_batch)?;
                load_account_history_append(collector, &mut writer).map_err(|e| {
                    base_execution_state_provider::ProviderError::other(Box::new(e))
                })?;
                Ok(((), writer.into_raw_rocksdb_batch()))
            })?;
        } else {
            let prepared = prepare_account_history_writes(collector, provider, &self.etl_config)?;
            provider.with_rocksdb_batch_auto_commit(|rocksdb_batch| {
                let mut writer = EitherWriter::new_accounts_history(provider, rocksdb_batch)?;
                write_prepared_history_shards::<tables::AccountsHistory>(
                    prepared,
                    |key, value| writer.upsert_account_history(key, value),
                )?;
                Ok(((), writer.into_raw_rocksdb_batch()))
            })?;
        }

        provider.commit_pending_rocksdb_batches()?;
        provider.rocksdb_provider().flush(&[Tables::AccountsHistory.name()])?;

        Ok(ExecOutput {
            checkpoint: StageCheckpoint::new(*block_range.end()),
            done: is_final_range,
        })
    }

    /// Unwind the stage.
    fn unwind(
        &mut self,
        provider: &Provider,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        let (range, unwind_progress, _) =
            input.unwind_block_range_with_threshold(self.commit_threshold);

        provider.unwind_account_history_indices_range(range)?;

        // from HistoryIndex higher than that number.
        Ok(UnwindOutput { checkpoint: StageCheckpoint::new(unwind_progress) })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use base_execution_state_database::{
        BlockNumberList,
        models::{AccountBeforeTx, ShardedKey, StoredBlockBodyIndices},
    };
    use base_execution_state_provider::DatabaseProviderFactory;

    use super::*;
    use crate::test_utils::TestStageDB;
    const ADDRESS: Address = address!("0x0000000000000000000000000000000000000001");
    const fn acc() -> AccountBeforeTx {
        AccountBeforeTx { address: ADDRESS, info: None }
    }
    const fn shard(shard_index: u64) -> ShardedKey<Address> {
        ShardedKey { key: ADDRESS, highest_block_number: shard_index }
    }
    fn list(list: &[u64]) -> BlockNumberList {
        BlockNumberList::new(list.iter().copied()).unwrap()
    }
    mod rocksdb_tests {
        use base_execution_state_provider::{
            RocksDBProviderFactory, StaticFileProviderFactory, providers::StaticFileWriter,
        };
        use base_execution_state_types::{StaticFileSegment, StorageSettings};

        use super::*;

        /// Sets up v2 account test data: writes block body indices to MDBX and
        /// account changesets to static files (matching realistic v2 layout).
        fn setup_v2_account_data(db: &TestStageDB, block_range: std::ops::RangeInclusive<u64>) {
            db.factory.set_storage_settings_cache(StorageSettings::v2());

            db.commit(|tx| {
                for block in block_range.clone() {
                    tx.put::<tables::BlockBodyIndices>(
                        block,
                        StoredBlockBodyIndices { tx_count: 3, ..Default::default() },
                    )?;
                }
                Ok(())
            })
            .unwrap();

            let static_file_provider = db.factory.static_file_provider();
            let mut writer =
                static_file_provider.latest_writer(StaticFileSegment::AccountChangeSets).unwrap();
            for block in block_range {
                writer.append_account_changeset(vec![acc()], block).unwrap();
            }
            writer.commit().unwrap();
        }

        /// Test that when `account_history_in_rocksdb` is enabled, the stage
        /// writes account history indices to `RocksDB` instead of MDBX.
        #[tokio::test]
        async fn execute_writes_to_rocksdb_when_enabled() {
            let db = TestStageDB::default();
            setup_v2_account_data(&db, 0..=10);

            let input = ExecInput { target: Some(10), ..Default::default() };
            let mut stage = IndexAccountHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(10), done: true });
            provider.commit().unwrap();

            // Verify MDBX table is empty (data should be in RocksDB)
            let mdbx_table = db.table::<tables::AccountsHistory>().unwrap();
            assert!(
                mdbx_table.is_empty(),
                "MDBX AccountsHistory should be empty when RocksDB is enabled"
            );

            // Verify RocksDB has the data
            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::AccountsHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should contain account history");

            let block_list = result.unwrap();
            let blocks: Vec<u64> = block_list.iter().collect();
            assert_eq!(blocks, (0..=10).collect::<Vec<_>>());
        }

        #[tokio::test]
        async fn prune_bump_rebuilds_empty_table_then_merges_later_chunks() {
            let db = TestStageDB::default();
            setup_v2_account_data(&db, 0..=10);
            let rocksdb = db.factory.rocksdb_provider();
            rocksdb.put::<tables::AccountsHistory>(shard(u64::MAX), &list(&[1])).unwrap();

            let mut stage = IndexAccountHistoryStage {
                commit_threshold: 2,
                prune_mode: Some(PruneMode::Before(6)),
                ..Default::default()
            };
            let mut checkpoint = None;
            for (threshold, expected_checkpoint, done) in
                [(2, 7, false), (2, 9, false), (u64::MAX, 20_000, true)]
            {
                stage.commit_threshold = threshold;
                let input = ExecInput { target: Some(20_000), checkpoint };
                let provider = db.factory.database_provider_rw().unwrap();
                let output = stage.execute(&provider, input).unwrap();
                assert_eq!(
                    output,
                    ExecOutput { checkpoint: StageCheckpoint::new(expected_checkpoint), done }
                );
                provider.commit().unwrap();
                checkpoint = Some(output.checkpoint);
            }

            let result = rocksdb.get::<tables::AccountsHistory>(shard(u64::MAX)).unwrap().unwrap();
            assert_eq!(result.iter().collect::<Vec<_>>(), (6..=10).collect::<Vec<_>>());
        }

        /// Test that unwind works correctly when `account_history_in_rocksdb` is enabled.
        #[tokio::test]
        async fn unwind_works_when_rocksdb_enabled() {
            let db = TestStageDB::default();
            setup_v2_account_data(&db, 0..=10);

            let input = ExecInput { target: Some(10), ..Default::default() };
            let mut stage = IndexAccountHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(10), done: true });
            provider.commit().unwrap();

            // Verify RocksDB has blocks 0-10 before unwind
            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::AccountsHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should have data before unwind");
            let blocks_before: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks_before, (0..=10).collect::<Vec<_>>());

            // Unwind to block 5 (remove blocks 6-10)
            let unwind_input =
                UnwindInput { checkpoint: StageCheckpoint::new(10), unwind_to: 5, bad_block: None };
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.unwind(&provider, unwind_input).unwrap();
            assert_eq!(out, UnwindOutput { checkpoint: StageCheckpoint::new(5) });
            provider.commit().unwrap();

            // Verify RocksDB now only has blocks 0-5 (blocks 6-10 removed)
            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::AccountsHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should still have data after unwind");
            let blocks_after: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks_after, (0..=5).collect::<Vec<_>>(), "Should only have blocks 0-5");
        }

        /// Test incremental sync merges new data with existing shards.
        #[tokio::test]
        async fn execute_incremental_sync() {
            let db = TestStageDB::default();
            setup_v2_account_data(&db, 0..=10);

            let input = ExecInput { target: Some(5), ..Default::default() };
            let mut stage = IndexAccountHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(5), done: true });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::AccountsHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some());
            let blocks: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks, (0..=5).collect::<Vec<_>>());

            let input = ExecInput { target: Some(10), checkpoint: Some(StageCheckpoint::new(5)) };
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(10), done: true });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::AccountsHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should have merged data");
            let blocks: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks, (0..=10).collect::<Vec<_>>());
        }
    }
}
