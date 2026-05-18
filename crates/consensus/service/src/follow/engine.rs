use std::{fmt::Debug, sync::Arc};

use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::{
    EngineClient, EngineState, EngineSyncStateUpdate, EngineTaskExt, InsertTask, SynchronizeTask,
};
use base_protocol::L2BlockInfo;
use tokio::sync::Mutex;

use crate::follow::error::FollowError;

#[async_trait]
pub(super) trait FollowEngine: Debug + Send + Sync {
    async fn insert_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Result<(), FollowError>;

    async fn update_safe_finalized_blocks(
        &self,
        safe: Option<L2BlockInfo>,
        finalized: Option<L2BlockInfo>,
    ) -> Result<(), FollowError>;
}

#[derive(Debug)]
pub(super) struct EngineApiFollowEngine<E: EngineClient> {
    client: Arc<E>,
    rollup_config: Arc<RollupConfig>,
    state: Mutex<EngineState>,
}

impl<E: EngineClient> EngineApiFollowEngine<E> {
    pub(super) fn new(
        client: Arc<E>,
        rollup_config: Arc<RollupConfig>,
        latest: L2BlockInfo,
        safe: L2BlockInfo,
        finalized: L2BlockInfo,
    ) -> Self {
        let mut state = EngineState::default();
        state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
            unsafe_head: Some(latest),
            local_safe_head: Some(safe),
            safe_head: Some(safe),
            finalized_head: Some(finalized),
        });
        Self { client, rollup_config, state: Mutex::new(state) }
    }
}

#[async_trait]
impl<E: EngineClient + Debug + 'static> FollowEngine for EngineApiFollowEngine<E> {
    async fn insert_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Result<(), FollowError> {
        let task = InsertTask::unsafe_payload(
            Arc::clone(&self.client),
            Arc::clone(&self.rollup_config),
            envelope,
        );
        task.execute(&mut *self.state.lock().await)
            .await
            .map_err(|e| FollowError::EngineTask(e.to_string()))
    }

    async fn update_safe_finalized_blocks(
        &self,
        safe: Option<L2BlockInfo>,
        finalized: Option<L2BlockInfo>,
    ) -> Result<(), FollowError> {
        if safe.is_none() && finalized.is_none() {
            return Ok(());
        }

        let task = SynchronizeTask::new(
            Arc::clone(&self.client),
            Arc::clone(&self.rollup_config),
            EngineSyncStateUpdate {
                local_safe_head: safe,
                safe_head: safe,
                finalized_head: finalized,
                ..Default::default()
            },
        );
        task.execute(&mut *self.state.lock().await)
            .await
            .map_err(|e| FollowError::EngineTask(e.to_string()))
    }
}
