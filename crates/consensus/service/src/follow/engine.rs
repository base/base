use std::{fmt::Debug, sync::Arc};

use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::{
    EngineClient, EngineState, EngineSyncStateUpdate, EngineTask, EngineTaskExt, InsertTask,
    SynchronizeTask,
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
        EngineTask::Insert(Box::new(task))
            .execute(&mut *self.state.lock().await)
            .await
            .map_err(FollowError::engine_task)
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
        task.execute(&mut *self.state.lock().await).await.map_err(FollowError::engine_task)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::{
        ExecutionPayloadV1, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
    };
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::test_utils::test_engine_client_builder;
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
    use tokio::time::{self, Instant};

    use super::{EngineApiFollowEngine, FollowEngine};

    fn valid_payload_status() -> PayloadStatus {
        PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(B256::ZERO) }
    }

    fn valid_forkchoice_updated() -> ForkchoiceUpdated {
        ForkchoiceUpdated { payload_status: valid_payload_status(), payload_id: None }
    }

    fn l1_info_deposit_tx() -> Vec<u8> {
        BaseTxEnvelope::from(TxDeposit {
            input: L1BlockInfoBedrock::default().encode_calldata(),
            ..Default::default()
        })
        .encoded_2718()
    }

    fn l2_block_info(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::with_last_byte(number as u8),
                number,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn payload(number: u64) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash: B256::with_last_byte(number.saturating_sub(1) as u8),
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash: B256::with_last_byte(number as u8),
                transactions: vec![l1_info_deposit_tx().into()],
            }),
        }
    }

    #[tokio::test]
    async fn insert_payload_retries_temporary_engine_errors() {
        let rollup_config = Arc::new(RollupConfig::default());
        let client = Arc::new(
            test_engine_client_builder()
                .with_config(Arc::clone(&rollup_config))
                .with_fork_choice_updated_v3_response(valid_forkchoice_updated())
                .build(),
        );
        let genesis = l2_block_info(0);
        let engine = Arc::new(EngineApiFollowEngine::new(
            Arc::clone(&client),
            rollup_config,
            genesis,
            genesis,
            genesis,
        ));

        let insert_engine = Arc::clone(&engine);
        let insert = tokio::spawn(async move { insert_engine.insert_payload(payload(1)).await });

        let deadline = Instant::now() + Duration::from_secs(1);
        while client.last_new_payload_v2().await.is_none() && Instant::now() < deadline {
            time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            client.last_new_payload_v2().await.is_some(),
            "follow insert should attempt engine_newPayload before retrying"
        );

        client.set_new_payload_v2_response(valid_payload_status()).await;

        time::timeout(Duration::from_secs(1), insert)
            .await
            .expect("insert should finish after temporary error clears")
            .expect("insert task should not panic")
            .expect("temporary engine error should be retried");
    }
}
