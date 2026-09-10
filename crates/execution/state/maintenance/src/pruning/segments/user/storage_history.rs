use alloy_primitives::BlockNumber;
use base_execution_state_database::DbTxMut;
use base_execution_state_provider::{
    DBProvider, RocksDBProviderFactory, StaticFileProviderFactory,
};
use base_execution_state_types::{
    PruneMode, PrunePurpose, PruneSegment, SegmentOutput, SegmentOutputCheckpoint,
    StaticFileSegment, StorageChangeSetReader, StorageSettingsCache,
};
use rustc_hash::FxHashMap;
use tracing::{instrument, trace};

use crate::pruning::{
    PrunerError,
    segments::{PruneInput, Segment},
};

#[derive(Debug)]
pub struct StorageHistory {
    mode: PruneMode,
}

impl StorageHistory {
    pub const fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl<Provider> Segment<Provider> for StorageHistory
where
    Provider: DBProvider<Tx: DbTxMut>
        + StaticFileProviderFactory
        + StorageChangeSetReader
        + StorageSettingsCache
        + RocksDBProviderFactory,
{
    fn segment(&self) -> PruneSegment {
        PruneSegment::StorageHistory
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "StorageHistory::prune",
        target = "pruner",
        skip(self, provider),
        ret(level = "trace")
    )]
    fn prune(&self, provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        let range = match input.get_next_block_range() {
            Some(range) => range,
            None => {
                trace!(target: "pruner", "No storage history to prune");
                return Ok(SegmentOutput::done());
            }
        };
        let range_end = *range.end();

        // Check where storage history indices are stored

        return self.prune_rocksdb(provider, input, range, range_end);
    }
}

impl StorageHistory {
    /// Prunes storage history when indices are stored in `RocksDB`.
    ///
    /// Reads storage changesets from static files and prunes the corresponding
    /// `RocksDB` history shards.
    fn prune_rocksdb<Provider>(
        &self,
        provider: &Provider,
        input: PruneInput,
        range: std::ops::RangeInclusive<BlockNumber>,
        range_end: BlockNumber,
    ) -> Result<SegmentOutput, PrunerError>
    where
        Provider: DBProvider + StaticFileProviderFactory + RocksDBProviderFactory,
    {
        let mut limiter = input.limiter;

        if limiter.is_limit_reached() {
            return Ok(SegmentOutput::not_done(
                limiter.interrupt_reason(),
                input.previous_checkpoint.map(SegmentOutputCheckpoint::from_prune_checkpoint),
            ));
        }

        let mut highest_deleted_storages: FxHashMap<_, _> = FxHashMap::default();
        let mut last_changeset_pruned_block = None;
        let mut changesets_processed = 0usize;
        let mut done = true;

        // Walk storage changesets from static files using a streaming iterator.
        // For each changeset, track the highest block number seen for each (address, storage_key)
        // pair to determine which history shard entries need pruning.
        let walker = provider.static_file_provider().walk_storage_changeset_range(range);
        for result in walker {
            let (block_address, entry) = result?;
            let block_number = block_address.block_number();
            let address = block_address.address();
            // Static file changesets are not deleted here, so an interrupted block cannot be
            // resumed: giving up the budget inside block N reports checkpoint N-1 and the next run
            // rereads the same entries, forever. Stop on block boundaries only, overshooting the
            // budget by at most the rest of one block.
            if limiter.is_limit_reached()
                && last_changeset_pruned_block.is_some_and(|last| last != block_number)
            {
                done = false;
                break;
            }
            highest_deleted_storages.insert((address, entry.key), block_number);
            last_changeset_pruned_block = Some(block_number);
            changesets_processed += 1;
            limiter.increment_deleted_entries_count();
        }

        trace!(target: "pruner", processed = %changesets_processed, %done, "Scanned storage changesets from static files");

        let last_changeset_pruned_block = last_changeset_pruned_block.unwrap_or(range_end);

        // Prune RocksDB history shards for affected storage slots
        let mut deleted_shards = 0usize;
        let mut updated_shards = 0usize;

        // Sort by (address, storage_key) for better RocksDB cache locality
        let mut sorted_storages: Vec<_> = highest_deleted_storages.into_iter().collect();
        sorted_storages.sort_unstable_by_key(|((addr, key), _)| (*addr, *key));

        provider.with_rocksdb_batch(|mut batch| {
            let targets: Vec<_> = sorted_storages
                .iter()
                .map(|((addr, key), highest)| {
                    ((*addr, *key), (*highest).min(last_changeset_pruned_block))
                })
                .collect();

            let outcomes = batch.prune_storage_history_batch(&targets)?;
            deleted_shards = outcomes.deleted;
            updated_shards = outcomes.updated;

            Ok(((), Some(batch.into_inner())))
        })?;

        trace!(target: "pruner", deleted = deleted_shards, updated = updated_shards, %done, "Pruned storage history (RocksDB indices)");

        // Delete static file jars only when fully processed. During provider.commit(), RocksDB
        // batch is committed before the MDBX checkpoint. If crash occurs after RocksDB commit
        // but before MDBX commit, on restart the pruner checkpoint indicates data needs
        // re-pruning, but the RocksDB shards are already pruned - this is safe because pruning
        // is idempotent (re-pruning already-pruned shards is a no-op).
        if done {
            provider.static_file_provider().delete_segment_below_block(
                StaticFileSegment::StorageChangeSets,
                last_changeset_pruned_block + 1,
            )?;
        }

        let progress = limiter.progress(done);

        Ok(SegmentOutput {
            progress,
            pruned: changesets_processed + deleted_shards + updated_shards,
            checkpoint: Some(SegmentOutputCheckpoint {
                block_number: Some(last_changeset_pruned_block),
                tx_number: None,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::B256;
    use assert_matches::assert_matches;
    use base_execution_state_database::{BlockNumberList, tables};
    use base_execution_state_provider::{
        DBProvider, DatabaseProviderFactory, PruneCheckpointReader,
    };
    use base_execution_state_types::{
        PruneCheckpoint, PruneMode, PruneProgress, PruneSegment, StorageSettingsCache,
    };
    use base_execution_sync::test_utils::{StorageKind, TestStageDB};
    use base_testing_support::{
        generators,
        generators::{BlockRangeParams, random_changeset_range, random_eoa_accounts},
    };

    use crate::pruning::segments::{
        PruneInput, PruneLimiter, Segment, SegmentOutput, StorageHistory,
    };

    #[test]
    fn prune_rocksdb() {
        use base_execution_state_database::models::storage_sharded_key::StorageShardedKey;
        use base_execution_state_provider::RocksDBProviderFactory;
        use base_execution_state_types::StorageSettings;

        let db = TestStageDB::default();
        let mut rng = generators::rng();

        let blocks = base_testing_support::BaseTestData::random_block_range(
            &mut rng,
            0..=100,
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: 0..1, ..Default::default() },
        );
        db.insert_blocks(blocks.iter(), StorageKind::Database(None)).expect("insert blocks");

        let accounts = random_eoa_accounts(&mut rng, 2).into_iter().collect::<BTreeMap<_, _>>();

        let (changesets, _) = random_changeset_range(
            &mut rng,
            blocks.iter(),
            accounts.into_iter().map(|(addr, acc)| (addr, (acc, Vec::new()))),
            1..2,
            1..2,
        );

        db.insert_changesets(changesets.clone(), None).expect("insert changesets to static files");

        let mut storage_indices: BTreeMap<(alloy_primitives::Address, B256), Vec<u64>> =
            BTreeMap::new();
        for (block, changeset) in changesets.iter().enumerate() {
            for (address, _, storage_entries) in changeset {
                for entry in storage_entries {
                    storage_indices.entry((*address, entry.key)).or_default().push(block as u64);
                }
            }
        }

        {
            let rocksdb = db.factory.rocksdb_provider();
            let mut batch = rocksdb.batch();
            for ((address, storage_key), block_numbers) in &storage_indices {
                let shard = BlockNumberList::new_pre_sorted(block_numbers.clone());
                batch
                    .put::<tables::StoragesHistory>(
                        StorageShardedKey::last(*address, *storage_key),
                        &shard,
                    )
                    .expect("insert storage history shard");
            }
            batch.commit().expect("commit rocksdb batch");
        }

        {
            let rocksdb = db.factory.rocksdb_provider();
            for (address, storage_key) in storage_indices.keys() {
                let shards = rocksdb.storage_history_shards(*address, *storage_key).unwrap();
                assert!(!shards.is_empty(), "RocksDB should contain storage history before prune");
            }
        }

        let to_block = 50u64;
        let prune_mode = PruneMode::Before(to_block);
        let input =
            PruneInput { previous_checkpoint: None, to_block, limiter: PruneLimiter::default() };
        let segment = StorageHistory::new(prune_mode);

        let provider = db.factory.database_provider_rw().unwrap();
        provider.set_storage_settings_cache(StorageSettings::v2());
        let result = segment.prune(&provider, input).unwrap();
        provider.commit().expect("commit");

        assert_matches!(
            result,
            SegmentOutput { progress: PruneProgress::Finished, checkpoint: Some(_), .. }
        );

        {
            let rocksdb = db.factory.rocksdb_provider();
            for ((address, storage_key), block_numbers) in &storage_indices {
                let shards = rocksdb.storage_history_shards(*address, *storage_key).unwrap();

                let remaining_blocks: Vec<u64> =
                    block_numbers.iter().copied().filter(|&b| b > to_block).collect();

                if remaining_blocks.is_empty() {
                    assert!(
                        shards.is_empty(),
                        "Shard for {:?}/{:?} should be deleted when all blocks pruned",
                        address,
                        storage_key
                    );
                } else {
                    assert!(!shards.is_empty(), "Shard should exist with remaining blocks");
                    let actual_blocks: Vec<u64> =
                        shards.iter().flat_map(|(_, list)| list.iter()).collect();
                    assert_eq!(
                        actual_blocks, remaining_blocks,
                        "RocksDB shard should only contain blocks > {}",
                        to_block
                    );
                }
            }
        }
    }

    /// A block holding at least a whole run's budget of changesets must not stall pruning: the
    /// walk deletes no changesets, so a checkpoint rewound below such a block would make every
    /// later run reread it and never advance.
    #[test]
    fn dense_block_advances_rocksdb_checkpoint() {
        use alloy_primitives::U256;
        use base_execution_state_database::models::storage_sharded_key::StorageShardedKey;
        use base_execution_state_provider::RocksDBProviderFactory;
        use base_execution_state_types::{StorageEntry, StorageSettings};

        let db = TestStageDB::default();
        let mut rng = generators::rng();

        let blocks = base_testing_support::BaseTestData::random_block_range(
            &mut rng,
            0..=20,
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: 0..1, ..Default::default() },
        );
        db.insert_blocks(blocks.iter(), StorageKind::Database(None)).expect("insert blocks");

        // Two storage changesets per block, so a budget of two makes every block "dense".
        const ENTRIES_PER_BLOCK: usize = 2;
        let (address, account) = random_eoa_accounts(&mut rng, 1).into_iter().next().unwrap();
        let keys = [B256::with_last_byte(1), B256::with_last_byte(2)];
        let changesets = (0..=20)
            .map(|_| {
                vec![(
                    address,
                    account,
                    keys.iter()
                        .map(|key| StorageEntry { key: *key, value: U256::from(1) })
                        .collect(),
                )]
            })
            .collect::<Vec<_>>();
        db.insert_changesets(changesets, None).expect("insert changesets to static files");

        {
            let rocksdb = db.factory.rocksdb_provider();
            let mut batch = rocksdb.batch();
            for key in keys {
                batch
                    .put::<tables::StoragesHistory>(
                        StorageShardedKey::last(address, key),
                        &BlockNumberList::new_pre_sorted(0..=20),
                    )
                    .expect("insert storage history shard");
            }
            batch.commit().expect("commit rocksdb batch");
        }

        let to_block = 15u64;
        let prune_mode = PruneMode::Before(to_block);
        let segment = StorageHistory::new(prune_mode);

        // Start from a checkpoint in the middle so a rewind can't be masked by block 0.
        let mut checkpoint = PruneCheckpoint { block_number: Some(4), tx_number: None, prune_mode };

        let run_prune = |checkpoint: PruneCheckpoint, limit: usize| {
            let input = PruneInput {
                previous_checkpoint: Some(checkpoint),
                to_block,
                limiter: PruneLimiter::default().set_deleted_entries_limit(limit),
            };

            let provider = db.factory.database_provider_rw().unwrap();
            provider.set_storage_settings_cache(StorageSettings::v2());
            let result = segment.prune(&provider, input).unwrap();
            segment
                .save_checkpoint(
                    &provider,
                    result.checkpoint.unwrap().as_prune_checkpoint(prune_mode),
                )
                .unwrap();
            provider.commit().expect("commit");

            let checkpoint = db
                .factory
                .provider()
                .unwrap()
                .get_prune_checkpoint(PruneSegment::StorageHistory)
                .unwrap()
                .unwrap();
            (result, checkpoint)
        };

        // The RocksDB path does not halve the limit, so this budget is exactly one dense block.
        for _ in 0..3 {
            let previous = checkpoint.block_number;
            let (result, next) = run_prune(checkpoint, ENTRIES_PER_BLOCK);
            checkpoint = next;

            assert!(
                !result.progress.is_finished(),
                "the range is longer than one run's budget allows"
            );
            assert!(
                checkpoint.block_number > previous,
                "checkpoint must advance past the dense block, got {:?} after {previous:?}",
                checkpoint.block_number
            );
        }
        assert_eq!(checkpoint.block_number, Some(7), "one dense block cleared per run");

        // With enough budget the remainder of the range completes in one run.
        let (result, checkpoint) = run_prune(checkpoint, 1000);
        assert!(result.progress.is_finished());
        assert_eq!(checkpoint.block_number, Some(to_block));
    }
}
