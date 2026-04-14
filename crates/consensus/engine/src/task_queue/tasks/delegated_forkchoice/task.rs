//! A follow-node task that applies delegated safe and finalized labels together.

use std::sync::Arc;

use async_trait::async_trait;
use base_consensus_genesis::RollupConfig;
use base_protocol::L2BlockInfo;
use derive_more::Constructor;

use crate::{
    ConsolidateInput, ConsolidateTask, DelegatedForkchoiceTaskError, EngineClient, EngineState,
    EngineTaskExt, FinalizeTask,
};

/// Delegated forkchoice labels from a remote follow source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegatedForkchoiceUpdate {
    /// The delegated safe L2 block.
    pub safe_l2: L2BlockInfo,
    /// The delegated finalized L2 block number, if available.
    pub finalized_l2_number: Option<u64>,
}

/// Applies delegated safe and finalized labels in engine-state order.
#[derive(Debug, Clone, Constructor)]
pub struct DelegatedForkchoiceTask<EngineClient_: EngineClient> {
    /// The engine client.
    pub client: Arc<EngineClient_>,
    /// The rollup config.
    pub cfg: Arc<RollupConfig>,
    /// The delegated labels to apply.
    pub update: DelegatedForkchoiceUpdate,
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for DelegatedForkchoiceTask<EngineClient_> {
    type Output = ();
    type Error = DelegatedForkchoiceTaskError;

    async fn execute(&self, state: &mut EngineState) -> Result<(), Self::Error> {
        ConsolidateTask::new(
            Arc::clone(&self.client),
            Arc::clone(&self.cfg),
            ConsolidateInput::BlockInfo(self.update.safe_l2),
        )
        .execute(state)
        .await?;

        let actual_safe = state.sync_state.safe_head().block_info.number;
        let Some(remote_finalized) = self.update.finalized_l2_number else {
            return Ok(());
        };

        let finalized_target = remote_finalized.min(actual_safe);
        let current_finalized = state.sync_state.finalized_head().block_info.number;
        if finalized_target <= current_finalized {
            debug!(
                target: "engine",
                actual_safe,
                current_finalized,
                finalized_target,
                "Skipping delegated finalized update"
            );
            return Ok(());
        }

        FinalizeTask::new(Arc::clone(&self.client), Arc::clone(&self.cfg), finalized_target)
            .execute(state)
            .await?;

        Ok(())
    }
}
