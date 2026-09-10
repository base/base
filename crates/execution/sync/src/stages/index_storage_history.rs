use std::fmt::Debug;

use base_execution_state_database::{DbTxMut, Tables, tables};
use base_execution_state_provider::{
    DBProvider, EitherWriter, HistoryWriter, PruneCheckpointReader, PruneCheckpointWriter,
    RocksDBProviderFactory, StaticFileProviderFactory, StorageChangeSetReader,
    StorageSettingsCache,
};
use base_execution_state_types::{
    EtlConfig, PruneCheckpoint, PruneMode, PrunePurpose, PruneSegment,
};
use tracing::info;

use super::collect_storage_history_indices;
use crate::{
    BlockRangeOutput, ExecInput, ExecOutput, IndexHistoryConfig, Stage, StageCheckpoint,
    StageError, StageId, UnwindInput, UnwindOutput,
    stages::utils::{
        load_storage_history_append, prepare_storage_history_writes, write_prepared_history_shards,
    },
};

/// Stage is indexing history the storage changesets generated in
/// [`ExecutionStage`][crate::stages::ExecutionStage]. For more information
/// on index sharding take a look at [`tables::StoragesHistory`].
#[derive(Debug)]
pub struct IndexStorageHistoryStage {
    /// Number of blocks after which the control
    /// flow will be returned to the pipeline for commit.
    pub commit_threshold: u64,
    /// Pruning configuration.
    pub prune_mode: Option<PruneMode>,
    /// ETL configuration
    pub etl_config: EtlConfig,
}

impl IndexStorageHistoryStage {
    /// Create new instance of [`IndexStorageHistoryStage`].
    pub const fn new(
        config: IndexHistoryConfig,
        etl_config: EtlConfig,
        prune_mode: Option<PruneMode>,
    ) -> Self {
        Self { commit_threshold: config.commit_threshold, etl_config, prune_mode }
    }
}

impl Default for IndexStorageHistoryStage {
    fn default() -> Self {
        Self { commit_threshold: 100_000, prune_mode: None, etl_config: EtlConfig::default() }
    }
}

impl<Provider> Stage<Provider> for IndexStorageHistoryStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + HistoryWriter
        + PruneCheckpointReader
        + PruneCheckpointWriter
        + StorageSettingsCache
        + RocksDBProviderFactory
        + StorageChangeSetReader
        + StaticFileProviderFactory,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::IndexStorageHistory
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
                    PruneSegment::StorageHistory,
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
            if provider.get_prune_checkpoint(PruneSegment::StorageHistory)?.is_none() {
                provider.save_prune_checkpoint(
                    PruneSegment::StorageHistory,
                    PruneCheckpoint {
                        block_number: Some(target_prunable_block),
                        tx_number: None,
                        prune_mode,
                    },
                )?;
            }
        }

        if input.target_reached() && prune_target_applied {
            provider.rocksdb_provider().clear::<tables::StoragesHistory>()?;

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
            provider.rocksdb_provider().clear::<tables::StoragesHistory>()?;

            if input.checkpoint().block_number == 0 && !prune_target_applied {
                block_range = 0..=*block_range.end();
            }
        }

        info!(target: "sync::stages::index_storage_history::exec", ?rebuild_from_empty, "Collecting indices");
        let collector =
            { collect_storage_history_indices(provider, block_range.clone(), &self.etl_config)? };

        info!(target: "sync::stages::index_storage_history::exec", "Loading indices into database");

        if rebuild_from_empty {
            provider.with_rocksdb_batch_auto_commit(|rocksdb_batch| {
                let mut writer = EitherWriter::new_storages_history(provider, rocksdb_batch)?;
                load_storage_history_append(collector, &mut writer).map_err(|e| {
                    base_execution_state_provider::ProviderError::other(Box::new(e))
                })?;
                Ok(((), writer.into_raw_rocksdb_batch()))
            })?;
        } else {
            let prepared = prepare_storage_history_writes(collector, provider, &self.etl_config)?;
            provider.with_rocksdb_batch_auto_commit(|rocksdb_batch| {
                let mut writer = EitherWriter::new_storages_history(provider, rocksdb_batch)?;
                write_prepared_history_shards::<tables::StoragesHistory>(
                    prepared,
                    |key, value| writer.upsert_storage_history(key, value),
                )?;
                Ok(((), writer.into_raw_rocksdb_batch()))
            })?;
        }

        provider.commit_pending_rocksdb_batches()?;
        provider.rocksdb_provider().flush(&[Tables::StoragesHistory.name()])?;

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

        provider.unwind_storage_history_indices_range(range)?;

        Ok(UnwindOutput { checkpoint: StageCheckpoint::new(unwind_progress) })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, address, b256};
    use base_execution_state_database::{
        BlockNumberList,
        models::{ShardedKey, StoredBlockBodyIndices, storage_sharded_key::StorageShardedKey},
    };
    use base_execution_state_provider::DatabaseProviderFactory;

    use super::*;
    use crate::test_utils::TestStageDB;
    const ADDRESS: Address = address!("0x0000000000000000000000000000000000000001");
    const STORAGE_KEY: B256 =
        b256!("0x0000000000000000000000000000000000000000000000000000000000000001");
    const fn shard(shard_index: u64) -> StorageShardedKey {
        StorageShardedKey {
            address: ADDRESS,
            sharded_key: ShardedKey { key: STORAGE_KEY, highest_block_number: shard_index },
        }
    }
    fn list(list: &[u64]) -> BlockNumberList {
        BlockNumberList::new(list.iter().copied()).unwrap()
    }
    mod rocksdb_tests {
        use base_execution_state_database::models::StorageBeforeTx;
        use base_execution_state_provider::{RocksDBProviderFactory, providers::StaticFileWriter};
        use base_execution_state_types::{StaticFileSegment, StorageSettings};

        use super::*;

        /// Sets up v2 storage test data: writes block body indices to MDBX and
        /// storage changesets to static files (matching realistic v2 layout).
        fn setup_v2_storage_data(db: &TestStageDB, block_range: std::ops::RangeInclusive<u64>) {
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
                static_file_provider.latest_writer(StaticFileSegment::StorageChangeSets).unwrap();
            for block in block_range {
                writer
                    .append_storage_changeset(
                        vec![StorageBeforeTx {
                            address: ADDRESS,
                            key: STORAGE_KEY,
                            value: U256::ZERO,
                        }],
                        block,
                    )
                    .unwrap();
            }
            writer.commit().unwrap();
        }

        /// Test that when `storages_history_in_rocksdb` is enabled, the stage
        /// writes storage history indices to `RocksDB` instead of MDBX.
        #[tokio::test]
        async fn execute_writes_to_rocksdb_when_enabled() {
            let db = TestStageDB::default();
            setup_v2_storage_data(&db, 0..=10);

            let input = ExecInput { target: Some(10), ..Default::default() };
            let mut stage = IndexStorageHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(10), done: true });
            provider.commit().unwrap();

            let mdbx_table = db.table::<tables::StoragesHistory>().unwrap();
            assert!(
                mdbx_table.is_empty(),
                "MDBX StoragesHistory should be empty when RocksDB is enabled"
            );

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should contain storage history");

            let block_list = result.unwrap();
            let blocks: Vec<u64> = block_list.iter().collect();
            assert_eq!(blocks, (0..=10).collect::<Vec<_>>());
        }

        #[tokio::test]
        async fn prune_bump_rebuilds_empty_table_then_merges_later_chunks() {
            let db = TestStageDB::default();
            setup_v2_storage_data(&db, 0..=10);
            let rocksdb = db.factory.rocksdb_provider();
            rocksdb.put::<tables::StoragesHistory>(shard(u64::MAX), &list(&[1])).unwrap();

            let mut stage = IndexStorageHistoryStage {
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

            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap().unwrap();
            assert_eq!(result.iter().collect::<Vec<_>>(), (6..=10).collect::<Vec<_>>());
        }

        /// Test that unwind works correctly when `storages_history_in_rocksdb` is enabled.
        #[tokio::test]
        async fn unwind_works_when_rocksdb_enabled() {
            let db = TestStageDB::default();
            setup_v2_storage_data(&db, 0..=10);

            let input = ExecInput { target: Some(10), ..Default::default() };
            let mut stage = IndexStorageHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(10), done: true });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should have data before unwind");
            let blocks_before: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks_before, (0..=10).collect::<Vec<_>>());

            let unwind_input =
                UnwindInput { checkpoint: StageCheckpoint::new(10), unwind_to: 5, bad_block: None };
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.unwind(&provider, unwind_input).unwrap();
            assert_eq!(out, UnwindOutput { checkpoint: StageCheckpoint::new(5) });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should still have data after partial unwind");
            let blocks_after: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(
                blocks_after,
                (0..=5).collect::<Vec<_>>(),
                "Should only have blocks 0-5 after unwind to block 5"
            );
        }

        /// Test that unwind to block 0 keeps only block 0's history.
        #[tokio::test]
        async fn unwind_to_zero_keeps_block_zero() {
            let db = TestStageDB::default();
            setup_v2_storage_data(&db, 0..=5);

            let input = ExecInput { target: Some(5), ..Default::default() };
            let mut stage = IndexStorageHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(5), done: true });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should have data before unwind");

            let unwind_input =
                UnwindInput { checkpoint: StageCheckpoint::new(5), unwind_to: 0, bad_block: None };
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.unwind(&provider, unwind_input).unwrap();
            assert_eq!(out, UnwindOutput { checkpoint: StageCheckpoint::new(0) });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should still have block 0 history");
            let blocks_after: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks_after, vec![0], "Should only have block 0 after unwinding to 0");
        }

        /// Test incremental sync merges new data with existing shards.
        #[tokio::test]
        async fn execute_incremental_sync() {
            let db = TestStageDB::default();
            setup_v2_storage_data(&db, 0..=10);

            let input = ExecInput { target: Some(5), ..Default::default() };
            let mut stage = IndexStorageHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(5), done: true });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some());
            let blocks: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks, (0..=5).collect::<Vec<_>>());

            let input = ExecInput { target: Some(10), checkpoint: Some(StageCheckpoint::new(5)) };
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(10), done: true });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let result = rocksdb.get::<tables::StoragesHistory>(shard(u64::MAX)).unwrap();
            assert!(result.is_some(), "RocksDB should have merged data");
            let blocks: Vec<u64> = result.unwrap().iter().collect();
            assert_eq!(blocks, (0..=10).collect::<Vec<_>>());
        }

        /// Test multi-shard unwind correctly handles shards that span across unwind boundary.
        #[tokio::test]
        async fn unwind_multi_shard() {
            use base_execution_state_database::models::sharded_key::NUM_OF_INDICES_IN_SHARD;

            let db = TestStageDB::default();
            let num_blocks = (NUM_OF_INDICES_IN_SHARD * 2 + 100) as u64;
            setup_v2_storage_data(&db, 0..=num_blocks - 1);

            let input = ExecInput { target: Some(num_blocks - 1), ..Default::default() };
            let mut stage = IndexStorageHistoryStage::default();
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.execute(&provider, input).unwrap();
            assert_eq!(
                out,
                ExecOutput { checkpoint: StageCheckpoint::new(num_blocks - 1), done: true }
            );
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let shards = rocksdb.storage_history_shards(ADDRESS, STORAGE_KEY).unwrap();
            assert!(shards.len() >= 2, "Should have at least 2 shards for {} blocks", num_blocks);

            let unwind_to = NUM_OF_INDICES_IN_SHARD as u64 + 50;
            let unwind_input = UnwindInput {
                checkpoint: StageCheckpoint::new(num_blocks - 1),
                unwind_to,
                bad_block: None,
            };
            let provider = db.factory.database_provider_rw().unwrap();
            let out = stage.unwind(&provider, unwind_input).unwrap();
            assert_eq!(out, UnwindOutput { checkpoint: StageCheckpoint::new(unwind_to) });
            provider.commit().unwrap();

            let rocksdb = db.factory.rocksdb_provider();
            let shards_after = rocksdb.storage_history_shards(ADDRESS, STORAGE_KEY).unwrap();
            assert!(!shards_after.is_empty(), "Should still have shards after unwind");

            let all_blocks: Vec<u64> =
                shards_after.iter().flat_map(|(_, list)| list.iter()).collect();
            assert_eq!(
                all_blocks,
                (0..=unwind_to).collect::<Vec<_>>(),
                "Should only have blocks 0 to {} after unwind",
                unwind_to
            );
        }
    }
}
