use alloy_primitives::BlockNumber;
use base_execution_state_types::{
    PruneMode, PrunePurpose, PruneSegment, SegmentOutput, SegmentOutputCheckpoint,
};
use reth_db_api::transaction::DbTxMut;
use reth_provider::{
    DBProvider, RocksDBProviderFactory, StaticFileProviderFactory,
    changeset_walker::StaticFileAccountChangesetWalker,
};
use reth_static_file_types::StaticFileSegment;
use reth_storage_api::{ChangeSetReader, StorageSettingsCache};
use rustc_hash::FxHashMap;
use tracing::{instrument, trace};

use crate::{
    PrunerError,
    segments::{PruneInput, Segment},
};

#[derive(Debug)]
pub struct AccountHistory {
    mode: PruneMode,
}

impl AccountHistory {
    pub const fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl<Provider> Segment<Provider> for AccountHistory
where
    Provider: DBProvider<Tx: DbTxMut>
        + StaticFileProviderFactory
        + StorageSettingsCache
        + ChangeSetReader
        + RocksDBProviderFactory,
{
    fn segment(&self) -> PruneSegment {
        PruneSegment::AccountHistory
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "AccountHistory::prune",
        target = "pruner",
        skip(self, provider),
        ret(level = "trace")
    )]
    fn prune(&self, provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        let range = match input.get_next_block_range() {
            Some(range) => range,
            None => {
                trace!(target: "pruner", "No account history to prune");
                return Ok(SegmentOutput::done());
            }
        };
        let range_end = *range.end();

        // Check where account history indices are stored

        return self.prune_rocksdb(provider, input, range, range_end);
    }
}

impl AccountHistory {
    /// Prunes account history when indices are stored in `RocksDB`.
    ///
    /// Reads account changesets from static files and prunes the corresponding
    /// `RocksDB` history shards.
    fn prune_rocksdb<Provider>(
        &self,
        provider: &Provider,
        input: PruneInput,
        range: std::ops::RangeInclusive<BlockNumber>,
        range_end: BlockNumber,
    ) -> Result<SegmentOutput, PrunerError>
    where
        Provider: DBProvider + StaticFileProviderFactory + ChangeSetReader + RocksDBProviderFactory,
    {
        // Unlike MDBX path, we don't divide the limit by 2 because RocksDB path only prunes
        // history shards (no separate changeset table to delete from). The changesets are in
        // static files which are deleted separately.
        let mut limiter = input.limiter;

        if limiter.is_limit_reached() {
            return Ok(SegmentOutput::not_done(
                limiter.interrupt_reason(),
                input.previous_checkpoint.map(SegmentOutputCheckpoint::from_prune_checkpoint),
            ));
        }

        let mut highest_deleted_accounts = FxHashMap::default();
        let mut last_changeset_pruned_block = None;
        let mut changesets_processed = 0usize;
        let mut done = true;

        // Walk account changesets from static files using a streaming iterator.
        // For each changeset, track the highest block number seen for each address
        // to determine which history shard entries need pruning.
        let walker = StaticFileAccountChangesetWalker::new(provider, range);
        for result in walker {
            let (block_number, changeset) = result?;
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
            highest_deleted_accounts.insert(changeset.address, block_number);
            last_changeset_pruned_block = Some(block_number);
            changesets_processed += 1;
            limiter.increment_deleted_entries_count();
        }
        trace!(target: "pruner", processed = %changesets_processed, %done, "Scanned account changesets from static files");

        let last_changeset_pruned_block = last_changeset_pruned_block.unwrap_or(range_end);

        // Prune RocksDB history shards for affected accounts
        let mut deleted_shards = 0usize;
        let mut updated_shards = 0usize;

        // Sort by address for better RocksDB cache locality
        let mut sorted_accounts: Vec<_> = highest_deleted_accounts.into_iter().collect();
        sorted_accounts.sort_unstable_by_key(|(addr, _)| *addr);

        provider.with_rocksdb_batch(|mut batch| {
            let targets: Vec<_> = sorted_accounts
                .iter()
                .map(|(addr, highest)| (*addr, (*highest).min(last_changeset_pruned_block)))
                .collect();

            let outcomes = batch.prune_account_history_batch(&targets)?;
            deleted_shards = outcomes.deleted;
            updated_shards = outcomes.updated;

            Ok(((), Some(batch.into_inner())))
        })?;
        trace!(target: "pruner", deleted = deleted_shards, updated = updated_shards, %done, "Pruned account history (RocksDB indices)");

        // Delete static file jars only when fully processed. During provider.commit(), RocksDB
        // batch is committed before the MDBX checkpoint. If crash occurs after RocksDB commit
        // but before MDBX commit, on restart the pruner checkpoint indicates data needs
        // re-pruning, but the RocksDB shards are already pruned - this is safe because pruning
        // is idempotent (re-pruning already-pruned shards is a no-op).
        if done {
            provider.static_file_provider().delete_segment_below_block(
                StaticFileSegment::AccountChangeSets,
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

    use alloy_primitives::{B256, BlockNumber};
    use assert_matches::assert_matches;
    use base_execution_state_types::{PruneCheckpoint, PruneMode, PruneProgress, PruneSegment};
    use reth_db_api::{BlockNumberList, models::StorageSettings, tables};
    use reth_provider::{DBProvider, DatabaseProviderFactory, PruneCheckpointReader};
    use reth_stages::test_utils::{StorageKind, TestStageDB};
    use reth_storage_api::StorageSettingsCache;
    use reth_testing_utils::generators::{
        self, BlockRangeParams, random_changeset_range, random_eoa_accounts,
    };

    use crate::segments::{AccountHistory, PruneInput, PruneLimiter, Segment, SegmentOutput};

    #[test]
    fn prune_rocksdb_path() {
        use reth_db_api::models::ShardedKey;
        use reth_provider::{RocksDBProviderFactory, StaticFileProviderFactory};

        let db = TestStageDB::default();
        let mut rng = generators::rng();

        let blocks = reth_testing_utils::BaseTestData::random_block_range(
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
            0..0,
            0..0,
        );

        db.insert_changesets(changesets.clone(), None).expect("insert changesets to static files");

        let mut account_blocks: BTreeMap<_, Vec<u64>> = BTreeMap::new();
        for (block, changeset) in changesets.iter().enumerate() {
            for (address, _, _) in changeset {
                account_blocks.entry(*address).or_default().push(block as u64);
            }
        }

        let rocksdb = db.factory.rocksdb_provider();
        let mut batch = rocksdb.batch();
        for (address, block_numbers) in &account_blocks {
            let shard = BlockNumberList::new_pre_sorted(block_numbers.iter().copied());
            batch
                .put::<tables::AccountsHistory>(ShardedKey::new(*address, u64::MAX), &shard)
                .unwrap();
        }
        batch.commit().unwrap();

        for (address, expected_blocks) in &account_blocks {
            let shards = rocksdb.account_history_shards(*address).unwrap();
            assert_eq!(shards.len(), 1);
            assert_eq!(shards[0].1.iter().collect::<Vec<_>>(), *expected_blocks);
        }

        let to_block: BlockNumber = 50;
        let prune_mode = PruneMode::Before(to_block);
        let input =
            PruneInput { previous_checkpoint: None, to_block, limiter: PruneLimiter::default() };
        let segment = AccountHistory::new(prune_mode);

        db.factory.set_storage_settings_cache(StorageSettings::v2());

        let provider = db.factory.database_provider_rw().unwrap();
        let result = segment.prune(&provider, input).unwrap();
        provider.commit().expect("commit");

        assert_matches!(
            result,
            SegmentOutput { progress: PruneProgress::Finished, pruned, checkpoint: Some(_) }
                if pruned > 0
        );

        for (address, original_blocks) in &account_blocks {
            let shards = rocksdb.account_history_shards(*address).unwrap();

            let expected_blocks: Vec<u64> =
                original_blocks.iter().copied().filter(|b| *b > to_block).collect();

            if expected_blocks.is_empty() {
                assert!(
                    shards.is_empty(),
                    "Expected no shards for address {address:?} after pruning"
                );
            } else {
                assert_eq!(shards.len(), 1, "Expected 1 shard for address {address:?}");
                assert_eq!(
                    shards[0].1.iter().collect::<Vec<_>>(),
                    expected_blocks,
                    "Shard blocks mismatch for address {address:?}"
                );
            }
        }

        let static_file_provider = db.factory.static_file_provider();
        let highest_block = static_file_provider.get_highest_static_file_block(
            reth_static_file_types::StaticFileSegment::AccountChangeSets,
        );
        if let Some(block) = highest_block {
            assert!(
                block > to_block,
                "Static files should only contain blocks above to_block ({to_block}), got {block}"
            );
        }
    }

    /// A block holding at least a whole run's budget of changesets must not stall the `RocksDB`
    /// path: the walk deletes no changesets, so a checkpoint rewound below such a block would make
    /// every later run reread it and never advance.
    #[test]
    fn dense_block_advances_rocksdb_checkpoint() {
        use reth_db_api::models::ShardedKey;
        use reth_provider::RocksDBProviderFactory;

        let db = TestStageDB::default();
        let mut rng = generators::rng();

        let blocks = reth_testing_utils::BaseTestData::random_block_range(
            &mut rng,
            0..=20,
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: 0..1, ..Default::default() },
        );
        db.insert_blocks(blocks.iter(), StorageKind::Database(None)).expect("insert blocks");

        let accounts = random_eoa_accounts(&mut rng, 2).into_iter().collect::<BTreeMap<_, _>>();
        let (changesets, _) = random_changeset_range(
            &mut rng,
            blocks.iter(),
            accounts.into_iter().map(|(addr, acc)| (addr, (acc, Vec::new()))),
            0..0,
            0..0,
        );
        // `random_changeset_range` emits exactly 2 account changesets per block (sender +
        // recipient), so a budget of 2 makes every block "dense".
        assert!(changesets.iter().all(|changeset| changeset.len() == 2));

        db.insert_changesets(changesets.clone(), None).expect("insert changesets to static files");

        // History index lives in RocksDB on the v2 path.
        let mut account_blocks: BTreeMap<_, Vec<u64>> = BTreeMap::new();
        for (block, changeset) in changesets.iter().enumerate() {
            for (address, _, _) in changeset {
                account_blocks.entry(*address).or_default().push(block as u64);
            }
        }
        let rocksdb = db.factory.rocksdb_provider();
        let mut batch = rocksdb.batch();
        for (address, block_numbers) in &account_blocks {
            let shard = BlockNumberList::new_pre_sorted(block_numbers.iter().copied());
            batch
                .put::<tables::AccountsHistory>(ShardedKey::new(*address, u64::MAX), &shard)
                .unwrap();
        }
        batch.commit().unwrap();

        db.factory.set_storage_settings_cache(StorageSettings::v2());

        let to_block: BlockNumber = 15;
        let prune_mode = PruneMode::Before(to_block);
        let segment = AccountHistory::new(prune_mode);

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
                .get_prune_checkpoint(PruneSegment::AccountHistory)
                .unwrap()
                .unwrap();
            (result, checkpoint)
        };

        // The RocksDB path does not halve the limit, so a budget of 2 is exactly one dense block.
        for _ in 0..3 {
            let previous = checkpoint.block_number;
            let (result, next) = run_prune(checkpoint, 2);
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
