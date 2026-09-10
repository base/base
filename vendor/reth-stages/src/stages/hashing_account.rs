use std::fmt::Debug;

use base_execution_state_database::{DbTxMut, tables};
use base_execution_state_provider::{DBProvider, HashingWriter, StatsReader};
use base_execution_state_types::ProviderResult;
use reth_stages_api::{
    EntitiesCheckpoint, ExecInput, ExecOutput, Stage, StageCheckpoint, StageError, StageId,
    UnwindInput, UnwindOutput,
};
use {base_execution_state_types::EtlConfig, reth_config::config::HashingConfig};

/// Advances the hashed-state checkpoint and restores hashed account state during unwind.
#[derive(Clone, Debug)]
pub struct AccountHashingStage {
    /// The threshold (in number of blocks) for switching between incremental
    /// hashing and full storage hashing.
    pub clean_threshold: u64,
    /// The maximum number of accounts to process before committing during unwind.
    pub commit_threshold: u64,
    /// The maximum number of changeset entries to process before committing. The stage commits
    /// after either `commit_threshold` blocks or `commit_entries` entries, whichever comes first.
    pub commit_entries: u64,
    /// ETL configuration
    pub etl_config: EtlConfig,
}

impl AccountHashingStage {
    /// Create new instance of [`AccountHashingStage`].
    pub const fn new(config: HashingConfig, etl_config: EtlConfig) -> Self {
        Self {
            clean_threshold: config.clean_threshold,
            commit_threshold: config.commit_threshold,
            commit_entries: config.commit_entries,
            etl_config,
        }
    }
}

impl Default for AccountHashingStage {
    fn default() -> Self {
        Self {
            clean_threshold: 500_000,
            commit_threshold: 100_000,
            commit_entries: 30_000_000,
            etl_config: EtlConfig::default(),
        }
    }
}

impl<Provider> Stage<Provider> for AccountHashingStage
where
    Provider: DBProvider<Tx: DbTxMut> + HashingWriter + StatsReader,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::AccountHashing
    }

    /// Execute the stage.
    ///
    /// Execution already writes hashed accounts, so this only advances the checkpoint.
    fn execute(
        &mut self,
        _provider: &Provider,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()));
        }

        // If using hashed state as canonical, execution already writes to `HashedAccounts`,
        // so this stage becomes a no-op.

        Ok(ExecOutput::done(input.checkpoint().with_block_number(input.target())))
    }

    /// Unwind the stage.
    fn unwind(
        &mut self,
        provider: &Provider,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        // Execution writes
        // directly to `HashedAccounts`, but the unwind must still revert those
        // entries here because `MerkleUnwind` runs after this stage (in unwind
        // order) and needs `HashedAccounts` to reflect the target block state
        // before it can verify the state root.
        let (range, unwind_progress, _) =
            input.unwind_block_range_with_threshold(self.commit_threshold);

        provider.unwind_account_hashing_range(range)?;

        let mut stage_checkpoint =
            input.checkpoint.account_hashing_stage_checkpoint().unwrap_or_default();

        stage_checkpoint.progress = stage_checkpoint_progress(provider)?;

        Ok(UnwindOutput {
            checkpoint: StageCheckpoint::new(unwind_progress)
                .with_account_hashing_stage_checkpoint(stage_checkpoint),
        })
    }
}

fn stage_checkpoint_progress(provider: &impl StatsReader) -> ProviderResult<EntitiesCheckpoint> {
    Ok(EntitiesCheckpoint {
        processed: provider.count_entries::<tables::HashedAccounts>()? as u64,
        total: provider.count_entries::<tables::HashedAccounts>()? as u64,
    })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_execution_state_database::{DbTx, DbTxMut, tables};
    use base_execution_state_memory::StoredAccount as Account;
    use base_execution_state_provider::test_utils::create_test_provider_factory;
    use reth_stages_api::{ExecInput, Stage, StageCheckpoint};

    use super::AccountHashingStage;

    #[test]
    fn execution_preserves_hashed_state_and_advances_checkpoint() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();
        let key = B256::with_last_byte(1);
        let value = Account { nonce: 7, ..Default::default() };
        provider.tx_ref().put::<tables::HashedAccounts>(key, value).unwrap();

        let output = AccountHashingStage::default()
            .execute(
                &*provider,
                ExecInput { target: Some(20), checkpoint: Some(StageCheckpoint::new(10)) },
            )
            .unwrap();

        assert!(output.done);
        assert_eq!(output.checkpoint.block_number, 20);
        assert_eq!(provider.tx_ref().get::<tables::HashedAccounts>(key).unwrap(), Some(value));
    }
}
