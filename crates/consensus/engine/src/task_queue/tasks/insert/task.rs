//! A task to insert a payload into the execution engine.

use std::{sync::Arc, time::Instant};

use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionPayloadInputV2, PayloadStatusEnum, PraguePayloadFields,
};
use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
};
use base_protocol::L2BlockInfo;

use crate::{
    EngineClient, EngineState, EngineTaskExt, InsertTaskError, SynchronizeTask,
    state::EngineSyncStateUpdate,
};

/// Whether inserting a payload should advance the safe head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPayloadSafety {
    /// Insert an unsafe payload.
    Unsafe,
    /// Insert a payload that is already safe.
    Safe,
}

impl InsertPayloadSafety {
    /// Returns true if this insert should advance the safe head.
    pub const fn advances_safe_head(self) -> bool {
        matches!(self, Self::Safe)
    }

    /// Returns the label used for structured logs.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Unsafe => "unsafe",
            Self::Safe => "safe",
        }
    }
}

/// The task to insert a payload into the execution engine.
#[derive(Debug, Clone)]
pub struct InsertTask<EngineClient_: EngineClient> {
    /// The engine client.
    client: Arc<EngineClient_>,
    /// The rollup config.
    rollup_config: Arc<RollupConfig>,
    /// The payload envelope.
    envelope: BaseExecutionPayloadEnvelope,
    /// Whether the inserted payload should advance the safe head.
    payload_safety: InsertPayloadSafety,
}

impl<EngineClient_: EngineClient> InsertTask<EngineClient_> {
    /// Creates a new insert task.
    pub const fn new(
        client: Arc<EngineClient_>,
        rollup_config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
        payload_safety: InsertPayloadSafety,
    ) -> Self {
        Self { client, rollup_config, envelope, payload_safety }
    }

    /// Creates a new task to insert an unsafe payload.
    pub const fn unsafe_payload(
        client: Arc<EngineClient_>,
        rollup_config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Self {
        Self::new(client, rollup_config, envelope, InsertPayloadSafety::Unsafe)
    }

    /// Creates a new task to insert a safe payload.
    pub const fn safe_payload(
        client: Arc<EngineClient_>,
        rollup_config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Self {
        Self::new(client, rollup_config, envelope, InsertPayloadSafety::Safe)
    }

    /// Checks the response of the `engine_newPayload` call.
    const fn check_new_payload_status(&self, status: &PayloadStatusEnum) -> bool {
        matches!(status, PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing)
    }

    fn is_unsafe_payload_applicable(
        &self,
        state: &EngineState,
        new_unsafe_ref: &L2BlockInfo,
    ) -> bool {
        if self.payload_safety.advances_safe_head() {
            return true;
        }

        let unsafe_head = state.sync_state.unsafe_head();
        if new_unsafe_ref.block_info.hash == unsafe_head.block_info.hash {
            debug!(
                target: "engine",
                hash = %new_unsafe_ref.block_info.hash,
                number = new_unsafe_ref.block_info.number,
                "Skipping already processed unsafe payload"
            );
            return false;
        }

        if new_unsafe_ref.block_info.number <= unsafe_head.block_info.number {
            info!(
                target: "engine",
                hash = %new_unsafe_ref.block_info.hash,
                number = new_unsafe_ref.block_info.number,
                unsafe_hash = %unsafe_head.block_info.hash,
                unsafe_number = unsafe_head.block_info.number,
                "Skipping unsafe payload older than current unsafe head"
            );
            return false;
        }

        if new_unsafe_ref.block_info.number == unsafe_head.block_info.number.saturating_add(1)
            && new_unsafe_ref.block_info.parent_hash != unsafe_head.block_info.hash
        {
            info!(
                target: "engine",
                hash = %new_unsafe_ref.block_info.hash,
                number = new_unsafe_ref.block_info.number,
                parent_hash = %new_unsafe_ref.block_info.parent_hash,
                unsafe_hash = %unsafe_head.block_info.hash,
                unsafe_number = unsafe_head.block_info.number,
                "Skipping unsafe payload that does not build onto current unsafe head"
            );
            return false;
        }

        true
    }
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for InsertTask<EngineClient_> {
    type Output = ();

    type Error = InsertTaskError;

    async fn execute(&self, state: &mut EngineState) -> Result<(), InsertTaskError> {
        let time_start = Instant::now();

        // Form a block ref before insertion so stale unsafe payloads can be dropped before import.
        let parent_beacon_block_root = self.envelope.parent_beacon_block_root.unwrap_or_default();
        let execution_payload = self.envelope.execution_payload.clone();
        let block: BaseBlock = match &execution_payload {
            BaseExecutionPayload::V1(payload) => BaseExecutionPayload::V1(payload.clone())
                .try_into_block()
                .map_err(InsertTaskError::FromBlockError)?,
            BaseExecutionPayload::V2(payload) => BaseExecutionPayload::V2(payload.clone())
                .try_into_block()
                .map_err(InsertTaskError::FromBlockError)?,
            BaseExecutionPayload::V3(payload) => BaseExecutionPayload::V3(payload.clone())
                .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v3(
                    CancunPayloadFields::new(parent_beacon_block_root, vec![]),
                ))
                .map_err(InsertTaskError::FromBlockError)?,
            BaseExecutionPayload::V4(payload) => BaseExecutionPayload::V4(payload.clone())
                .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v4(
                    CancunPayloadFields::new(parent_beacon_block_root, vec![]),
                    PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
                ))
                .map_err(InsertTaskError::FromBlockError)?,
        };

        let new_block_ref =
            L2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
                .map_err(InsertTaskError::L2BlockInfoConstruction)?;

        if !self.is_unsafe_payload_applicable(state, &new_block_ref) {
            return Ok(());
        }

        // Insert the new payload.
        let insert_time_start = Instant::now();
        let response = match execution_payload {
            BaseExecutionPayload::V1(payload) => {
                let payload_input =
                    ExecutionPayloadInputV2 { execution_payload: payload, withdrawals: None };
                self.client.new_payload_v2(payload_input).await
            }
            BaseExecutionPayload::V2(payload) => {
                let payload_input = ExecutionPayloadInputV2 {
                    execution_payload: payload.payload_inner,
                    withdrawals: Some(payload.withdrawals),
                };
                self.client.new_payload_v2(payload_input).await
            }
            BaseExecutionPayload::V3(payload) => {
                self.client.new_payload_v3(payload, parent_beacon_block_root).await
            }
            BaseExecutionPayload::V4(payload) => {
                self.client.new_payload_v4(payload, parent_beacon_block_root).await
            }
        };

        // Check the `engine_newPayload` response.
        let response = match response {
            Ok(resp) => resp,
            Err(e) => {
                warn!(
                    target: "engine",
                    error = %e,
                    payload_safety = self.payload_safety.as_label(),
                    "Failed to insert new payload"
                );
                return Err(InsertTaskError::InsertFailed(e));
            }
        };
        if !self.check_new_payload_status(&response.status) {
            return Err(InsertTaskError::UnexpectedPayloadStatus(response.status));
        }
        let insert_duration = insert_time_start.elapsed();

        let advances_safe_head = self.payload_safety.advances_safe_head();
        // Send a FCU to canonicalize the imported block.
        SynchronizeTask::new(
            Arc::clone(&self.client),
            Arc::clone(&self.rollup_config),
            EngineSyncStateUpdate {
                unsafe_head: Some(new_block_ref),
                local_safe_head: advances_safe_head.then_some(new_block_ref),
                safe_head: advances_safe_head.then_some(new_block_ref),
                ..Default::default()
            },
        )
        .execute(state)
        .await?;

        let total_duration = time_start.elapsed();

        info!(
            target: "engine",
            hash = %new_block_ref.block_info.hash,
            number = new_block_ref.block_info.number,
            payload_safety = self.payload_safety.as_label(),
            total_duration = ?total_duration,
            insert_duration = ?insert_duration,
            "Inserted new payload"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bloom, FixedBytes, U256};
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};

    use super::{InsertPayloadSafety, InsertTask};
    use crate::{
        EngineTaskExt,
        test_utils::{TestEngineStateBuilder, test_engine_client_builder},
    };

    fn valid_payload_status() -> PayloadStatus {
        PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(FixedBytes::ZERO),
        }
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

    fn l2_block_info(block_number: u64, hash: B256, parent_hash: B256) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash,
                number: block_number,
                parent_hash,
                timestamp: block_number,
            },
            l1_origin: Default::default(),
            seq_num: 0,
        }
    }

    fn bedrock_payload_with_parent(block_number: u64, parent_hash: B256) -> BaseExecutionPayload {
        BaseExecutionPayload::V1(alloy_rpc_types_engine::ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: 1,
            extra_data: Default::default(),
            base_fee_per_gas: U256::ZERO,
            block_hash: B256::with_last_byte(block_number as u8),
            transactions: vec![l1_info_deposit_tx().into()],
        })
    }

    fn bedrock_payload(block_number: u64) -> BaseExecutionPayload {
        bedrock_payload_with_parent(block_number, B256::ZERO)
    }

    fn canyon_payload(block_number: u64) -> BaseExecutionPayload {
        BaseExecutionPayload::V2(alloy_rpc_types_engine::ExecutionPayloadV2 {
            payload_inner: alloy_rpc_types_engine::ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1_704_992_401,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash: B256::with_last_byte(block_number as u8),
                transactions: vec![l1_info_deposit_tx().into()],
            },
            withdrawals: vec![],
        })
    }

    fn test_client() -> Arc<crate::test_utils::MockEngineClient> {
        Arc::new(
            test_engine_client_builder()
                .with_new_payload_v2_response(valid_payload_status())
                .with_fork_choice_updated_v3_response(valid_forkchoice_updated())
                .build(),
        )
    }

    #[tokio::test]
    async fn bedrock_payload_uses_new_payload_v2_with_no_withdrawals() {
        let client = test_client();
        let payload = bedrock_payload(1);
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: payload,
        };
        let mut state = TestEngineStateBuilder::new().build();

        InsertTask::new(
            Arc::clone(&client),
            Arc::new(base_common_genesis::RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
        )
        .execute(&mut state)
        .await
        .expect("bedrock payload should be imported with engine_newPayloadV2");

        let payload_input = client
            .last_new_payload_v2()
            .await
            .expect("new_payload_v2 should record the payload input");
        assert!(
            payload_input.withdrawals.is_none(),
            "bedrock payload must keep withdrawals unset when sent via engine_newPayloadV2"
        );
    }

    #[tokio::test]
    async fn canyon_payload_uses_new_payload_v2_with_withdrawals() {
        let client = test_client();
        let payload = canyon_payload(1);
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: payload,
        };
        let mut state = TestEngineStateBuilder::new().build();

        InsertTask::new(
            Arc::clone(&client),
            Arc::new(base_common_genesis::RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
        )
        .execute(&mut state)
        .await
        .expect("canyon payload should be imported with engine_newPayloadV2");

        let payload_input = client
            .last_new_payload_v2()
            .await
            .expect("new_payload_v2 should record the payload input");
        assert_eq!(
            payload_input.withdrawals,
            Some(vec![]),
            "canyon payload must preserve withdrawals when sent via engine_newPayloadV2"
        );
    }

    #[tokio::test]
    async fn unsafe_payload_insert_advances_only_unsafe_head() {
        let client = test_client();
        let payload = bedrock_payload(2);
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: payload,
        };
        let mut state = TestEngineStateBuilder::new().build();

        InsertTask::unsafe_payload(
            Arc::clone(&client),
            Arc::new(base_common_genesis::RollupConfig::default()),
            envelope,
        )
        .execute(&mut state)
        .await
        .expect("unsafe payload should be inserted");

        assert_eq!(state.sync_state.unsafe_head().block_info.number, 2);
        assert_eq!(state.sync_state.local_safe_head().block_info.number, 0);
        assert_eq!(state.sync_state.safe_head().block_info.number, 0);
    }

    #[tokio::test]
    async fn safe_payload_insert_advances_safe_heads() {
        let client = test_client();
        let payload = bedrock_payload(3);
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: payload,
        };
        let mut state = TestEngineStateBuilder::new().build();

        InsertTask::safe_payload(
            Arc::clone(&client),
            Arc::new(base_common_genesis::RollupConfig::default()),
            envelope,
        )
        .execute(&mut state)
        .await
        .expect("safe payload should be inserted");

        assert_eq!(state.sync_state.unsafe_head().block_info.number, 3);
        assert_eq!(state.sync_state.local_safe_head().block_info.number, 3);
        assert_eq!(state.sync_state.safe_head().block_info.number, 3);
    }

    #[tokio::test]
    async fn stale_unsafe_payload_is_dropped_before_new_payload() {
        let client = test_client();
        let current_unsafe = l2_block_info(4, B256::with_last_byte(4), B256::with_last_byte(3));
        let mut state = TestEngineStateBuilder::new().with_unsafe_head(current_unsafe).build();
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload_with_parent(2, B256::with_last_byte(1)),
        };

        InsertTask::unsafe_payload(
            Arc::clone(&client),
            Arc::new(base_common_genesis::RollupConfig::default()),
            envelope,
        )
        .execute(&mut state)
        .await
        .expect("stale unsafe payload should be dropped without retrying");

        assert!(
            client.last_new_payload_v2().await.is_none(),
            "stale unsafe payload should not be sent to engine_newPayload"
        );
        assert_eq!(state.sync_state.unsafe_head(), current_unsafe);
    }

    #[tokio::test]
    async fn next_unsafe_payload_with_wrong_parent_is_dropped_before_new_payload() {
        let client = test_client();
        let current_unsafe = l2_block_info(4, B256::with_last_byte(4), B256::with_last_byte(3));
        let mut state = TestEngineStateBuilder::new().with_unsafe_head(current_unsafe).build();
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload_with_parent(5, B256::with_last_byte(0x99)),
        };

        InsertTask::unsafe_payload(
            Arc::clone(&client),
            Arc::new(base_common_genesis::RollupConfig::default()),
            envelope,
        )
        .execute(&mut state)
        .await
        .expect("wrong-parent unsafe payload should be dropped without retrying");

        assert!(
            client.last_new_payload_v2().await.is_none(),
            "wrong-parent unsafe payload should not be sent to engine_newPayload"
        );
        assert_eq!(state.sync_state.unsafe_head(), current_unsafe);
    }

    #[tokio::test]
    async fn direct_child_unsafe_payload_is_inserted() {
        let client = test_client();
        let current_unsafe = l2_block_info(4, B256::with_last_byte(4), B256::with_last_byte(3));
        let mut state = TestEngineStateBuilder::new().with_unsafe_head(current_unsafe).build();
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload_with_parent(5, current_unsafe.block_info.hash),
        };

        InsertTask::unsafe_payload(
            Arc::clone(&client),
            Arc::new(base_common_genesis::RollupConfig::default()),
            envelope,
        )
        .execute(&mut state)
        .await
        .expect("direct-child unsafe payload should be inserted");

        assert!(
            client.last_new_payload_v2().await.is_some(),
            "direct-child unsafe payload should be sent to engine_newPayload"
        );
        assert_eq!(state.sync_state.unsafe_head().block_info.number, 5);
        assert_eq!(state.sync_state.unsafe_head().block_info.parent_hash, current_unsafe.hash());
    }
}
