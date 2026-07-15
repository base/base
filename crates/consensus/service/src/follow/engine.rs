use std::{fmt::Debug, sync::Arc};

use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::{
    EngineClient, EngineState, EngineSyncStateUpdate, EngineTask, EngineTaskError,
    EngineTaskErrorSeverity, EngineTaskExt, InsertTask, SynchronizeTask,
};
use base_protocol::L2BlockInfo;
use tokio::{sync::Mutex, task::yield_now};

use crate::follow::error::FollowError;

#[async_trait]
pub(super) trait FollowEngine: Debug + Send + Sync {
    async fn insert_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Result<L2BlockInfo, FollowError>;

    async fn update_safe_finalized_blocks(
        &self,
        safe: Option<L2BlockInfo>,
        finalized: Option<L2BlockInfo>,
    ) -> Result<(), FollowError>;

    /// Reorg the local unsafe and safe heads down to `ancestor` — a block the EL already has, so
    /// the forkchoice update returns Valid and the head reorgs immediately (no EL sync). Refuses to
    /// reorg below the finalized head.
    async fn reset_to_ancestor(&self, ancestor: L2BlockInfo) -> Result<(), FollowError>;
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
    ) -> Result<L2BlockInfo, FollowError> {
        let task = InsertTask::unsafe_payload(
            Arc::clone(&self.client),
            Arc::clone(&self.rollup_config),
            envelope,
        );
        let mut state = self.state.lock().await;
        EngineTask::Insert(Box::new(task))
            .execute(&mut state)
            .await
            .map_err(FollowError::engine_task)?;
        Ok(state.sync_state.unsafe_head())
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

    async fn reset_to_ancestor(&self, ancestor: L2BlockInfo) -> Result<(), FollowError> {
        let mut state = self.state.lock().await;

        // Never rewind below finality. `apply_update` does not enforce this, so guard here.
        let finalized = state.sync_state.finalized_head().block_info.number;
        if ancestor.block_info.number < finalized {
            return Err(FollowError::ReorgBelowFinalized {
                number: ancestor.block_info.number,
                finalized,
            });
        }

        // `ancestor` is a block the local EL already has (it is on the local chain), so this
        // forkchoice update returns Valid and reorgs the unsafe head down to it without EL sync.
        // Replaying source payloads from ancestor+1 then rebuilds onto the source's chain.
        let task = SynchronizeTask::new(
            Arc::clone(&self.client),
            Arc::clone(&self.rollup_config),
            EngineSyncStateUpdate {
                unsafe_head: Some(ancestor),
                local_safe_head: Some(ancestor),
                safe_head: Some(ancestor),
                ..Default::default()
            },
        );
        loop {
            match task.execute(&mut state).await {
                Ok(()) => break,
                Err(error) if error.severity() == EngineTaskErrorSeverity::Temporary => {
                    // A transport error may mean the EL applied the FCU but the response was
                    // lost. Repeating the same FCU reconciles the in-memory state once the EL
                    // responds again.
                    yield_now().await;
                }
                Err(error) => return Err(FollowError::engine_task(error)),
            }
        }

        if state.sync_state.unsafe_head().block_info.hash != ancestor.block_info.hash {
            return Err(FollowError::ResetToAncestorUnconfirmed {
                number: ancestor.block_info.number,
                hash: ancestor.block_info.hash,
            });
        }

        Ok(())
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
    use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::test_utils::test_engine_client_builder;
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
    use tokio::time::{self, Instant};

    use super::{EngineApiFollowEngine, FollowEngine};
    use crate::follow::error::FollowError;

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

    fn l2_block(number: u64, hash: B256) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { hash, number, ..Default::default() },
            ..Default::default()
        }
    }

    /// Builds a canonical chain of `len` blocks (numbered `1..=len`) where each block's parent is the
    /// previous block's *computed* hash and the declared `block_hash` equals the computed hash — as a
    /// real EL would have it. This keeps the stateful mock (which keys off the declared `block_hash`)
    /// consistent with `InsertTask`, which recomputes the hash from the block body for its forkchoice
    /// update. Returns each payload paired with its computed [`L2BlockInfo`].
    fn canonical_chain(
        rollup_config: &RollupConfig,
        len: u64,
    ) -> Vec<(BaseExecutionPayloadEnvelope, L2BlockInfo)> {
        let mut chain = Vec::new();
        let mut parent = B256::ZERO;
        for number in 1..=len {
            let mut env = payload(number);
            let BaseExecutionPayload::V1(p) = &mut env.execution_payload else { unreachable!() };
            p.parent_hash = parent;
            let block: BaseBlock = env.execution_payload.clone().try_into_block().expect("block");
            let info = L2BlockInfo::from_block_and_genesis(&block, &rollup_config.genesis)
                .expect("block info");
            let BaseExecutionPayload::V1(p) = &mut env.execution_payload else { unreachable!() };
            p.block_hash = info.block_info.hash;
            parent = info.block_info.hash;
            chain.push((env, info));
        }
        chain
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

    #[tokio::test]
    async fn reset_to_ancestor_retries_temporary_forkchoice_errors() {
        let rollup_config = Arc::new(RollupConfig::default());
        let client =
            Arc::new(test_engine_client_builder().with_config(Arc::clone(&rollup_config)).build());
        let ancestor = l2_block_info(1);
        let engine = Arc::new(EngineApiFollowEngine::new(
            Arc::clone(&client),
            rollup_config,
            l2_block_info(2),
            ancestor,
            ancestor,
        ));

        let reset_engine = Arc::clone(&engine);
        let reset = tokio::spawn(async move { reset_engine.reset_to_ancestor(ancestor).await });
        time::sleep(Duration::from_millis(10)).await;
        client.set_fork_choice_updated_v3_response(valid_forkchoice_updated()).await;

        time::timeout(Duration::from_secs(1), reset)
            .await
            .expect("reset should finish after temporary error clears")
            .expect("reset task should not panic")
            .expect("temporary forkchoice error should be retried");
    }

    #[tokio::test]
    async fn reset_to_ancestor_then_replay_rebuilds_onto_source() {
        let rollup_config = Arc::new(RollupConfig::default());
        // Canonical source chain, blocks 1..=3, with consistent computed hashes.
        let chain = canonical_chain(&rollup_config, 3);
        let block1 = chain[0].1;
        let bad2 = B256::with_last_byte(102);

        // The EL is on a bad fork: it has the shared prefix up to canonical block 1 plus a bad block
        // 2, with its head on the bad tip.
        let client = Arc::new(
            test_engine_client_builder()
                .with_config(Arc::clone(&rollup_config))
                .with_stateful_el([block1.block_info.hash, bad2], bad2)
                .build(),
        );
        let engine = Arc::new(EngineApiFollowEngine::new(
            Arc::clone(&client),
            Arc::clone(&rollup_config),
            l2_block(2, bad2), // latest (bad tip)
            block1,            // safe
            block1,            // finalized at block 1
        ));

        // Reset to the common ancestor (canonical block 1), a block the EL already has, so the
        // forkchoice update is Valid and the head reorgs down to it without EL sync.
        engine.reset_to_ancestor(block1).await.expect("reset to ancestor");
        assert_eq!(
            client.stateful_head().await,
            Some(block1.block_info.hash),
            "head should reorg down to the ancestor"
        );

        // Replay the canonical chain from ancestor+1. Each payload's parent is already known, so the
        // EL accepts it (Valid) and the head advances onto the source chain.
        engine.insert_payload(chain[1].0.clone()).await.expect("replay block 2");
        engine.insert_payload(chain[2].0.clone()).await.expect("replay block 3");

        assert_eq!(
            client.stateful_head().await,
            Some(chain[2].1.block_info.hash),
            "head should be rebuilt onto the canonical chain"
        );
    }

    #[tokio::test]
    async fn reset_to_unknown_tip_returns_syncing_and_does_not_recover() {
        let rollup_config = Arc::new(RollupConfig::default());
        let h0 = B256::with_last_byte(0);
        let h1 = B256::with_last_byte(1);
        let bad2 = B256::with_last_byte(102);
        let client = Arc::new(
            test_engine_client_builder()
                .with_config(Arc::clone(&rollup_config))
                .with_stateful_el([h0, h1, bad2], bad2)
                .build(),
        );
        let engine = Arc::new(EngineApiFollowEngine::new(
            Arc::clone(&client),
            Arc::clone(&rollup_config),
            l2_block(2, bad2),
            l2_block(1, h1),
            l2_block(0, h0),
        ));

        // Pointing the EL at the canonical *tip* (a block it does not have) returns Syncing: the
        // head does not move and nothing is recovered. This is exactly why recovery must target the
        // common ancestor, not the source tip.
        let tip = l2_block(5, B256::with_last_byte(205));
        let error = engine
            .reset_to_ancestor(tip)
            .await
            .expect_err("syncing must be surfaced as a failed reset");
        assert!(matches!(error, FollowError::ResetToAncestorUnconfirmed { number: 5, .. }));
        assert_eq!(
            client.stateful_head().await,
            Some(bad2),
            "head must not advance to a block the EL lacks"
        );
    }

    #[tokio::test]
    async fn reset_below_finalized_is_refused() {
        let rollup_config = Arc::new(RollupConfig::default());
        let h5 = B256::with_last_byte(5);
        let head = B256::with_last_byte(8);
        let client = Arc::new(
            test_engine_client_builder()
                .with_config(Arc::clone(&rollup_config))
                .with_stateful_el([head], head)
                .build(),
        );
        let engine = Arc::new(EngineApiFollowEngine::new(
            Arc::clone(&client),
            Arc::clone(&rollup_config),
            l2_block(8, head),
            l2_block(5, h5),
            l2_block(5, h5), // finalized at block 5
        ));

        let error = engine
            .reset_to_ancestor(l2_block(4, B256::with_last_byte(4)))
            .await
            .expect_err("must refuse to rewind below finalized");
        assert!(matches!(error, FollowError::ReorgBelowFinalized { number: 4, finalized: 5 }));
        assert_eq!(
            client.stateful_head().await,
            Some(head),
            "no forkchoice update should be issued on refusal"
        );
    }
}
