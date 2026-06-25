//! Direct follow-node delegated forkchoice operations.

use std::sync::Arc;

use base_common_genesis::RollupConfig;
use base_protocol::L2BlockInfo;
use thiserror::Error;

use crate::{
    ConsolidateInput, ConsolidateTaskError, Engine, EngineClient, EngineState, EngineTaskError,
    EngineTaskErrorSeverity, FinalizeTaskError, Metrics,
};

/// An error returned by a delegated follow-node forkchoice update.
#[derive(Debug, Error)]
pub enum DelegatedForkchoiceTaskError {
    /// Consolidation failed while applying the delegated safe head.
    #[error(transparent)]
    Consolidate(#[from] ConsolidateTaskError),
    /// Finalization failed while advancing the delegated finalized head.
    #[error(transparent)]
    Finalize(#[from] FinalizeTaskError),
}

impl EngineTaskError for DelegatedForkchoiceTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::Consolidate(inner) => inner.severity(),
            Self::Finalize(inner) => inner.severity(),
        }
    }
}

/// Delegated forkchoice labels from a remote follow source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegatedForkchoiceUpdate {
    /// The delegated safe L2 block.
    pub safe_l2: L2BlockInfo,
    /// The delegated finalized L2 block number, if available.
    pub finalized_l2_number: Option<u64>,
}

impl Engine {
    /// Applies delegated safe and finalized labels directly against the execution layer.
    pub async fn delegated_forkchoice<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        update: DelegatedForkchoiceUpdate,
    ) -> Result<(), DelegatedForkchoiceTaskError>
    where
        EngineClient_: EngineClient + 'static,
    {
        self.retry_with_severity(Metrics::DELEGATED_FORKCHOICE_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            Box::pin(async move {
                Self::delegated_forkchoice_with_state(state, client, config, update).await
            })
        })
        .await
    }

    /// Applies delegated safe and finalized labels using the provided engine state.
    pub async fn delegated_forkchoice_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        update: DelegatedForkchoiceUpdate,
    ) -> Result<(), DelegatedForkchoiceTaskError> {
        if state.sync_state.safe_head() != update.safe_l2 {
            let input = ConsolidateInput::BlockInfo(update.safe_l2);
            Self::consolidate_with_state(state, Arc::clone(&client), Arc::clone(&config), &input)
                .await?;
        } else {
            debug!(
                target: "engine",
                safe_hash = %update.safe_l2.block_info.hash,
                safe_number = update.safe_l2.block_info.number,
                "Skipping delegated safe update already reflected in engine state"
            );
        }

        let actual_safe = state.sync_state.safe_head().block_info.number;
        let Some(remote_finalized) = update.finalized_l2_number else { return Ok(()) };

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

        Self::finalize_with_state(state, client, config, finalized_target).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
    use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_rpc_types::Transaction as BaseTransaction;
    use base_protocol::{BlockInfo, L1BlockInfoBedrock};

    use super::*;
    use crate::test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder};

    fn syncing_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Syncing,
                latest_valid_hash: None,
            },
            payload_id: None,
        }
    }

    fn l1_info_deposit_tx() -> BaseTxEnvelope {
        BaseTxEnvelope::from(TxDeposit {
            input: L1BlockInfoBedrock::default().encode_calldata(),
            ..Default::default()
        })
    }

    fn rpc_transaction(tx: BaseTxEnvelope, block_number: u64) -> BaseTransaction {
        BaseTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(tx, Address::ZERO),
                block_hash: None,
                block_number: Some(block_number),
                block_timestamp: None,
                effective_gas_price: Some(0),
                transaction_index: Some(0),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    fn block_with_hash(number: u64, hash: B256) -> RpcBlock<BaseTransaction> {
        let mut block = RpcBlock::<BaseTransaction>::default();
        block.header.hash = hash;
        block.header.inner.number = number;
        block.header.inner.timestamp = number * 2;
        block.transactions =
            BlockTransactions::Full(vec![rpc_transaction(l1_info_deposit_tx(), number)]);
        block
    }

    #[tokio::test]
    async fn syncing_safe_update_skips_finalization_beyond_actual_safe() {
        let delegated_safe_number = 80;
        let delegated_safe_hash = B256::from([0x11; 32]);
        let delegated_safe = L2BlockInfo {
            block_info: BlockInfo {
                hash: delegated_safe_hash,
                number: delegated_safe_number,
                ..Default::default()
            },
            ..Default::default()
        };

        let client = Arc::new(
            test_engine_client_builder()
                .with_l2_block_by_label(
                    BlockNumberOrTag::Number(delegated_safe_number),
                    block_with_hash(delegated_safe_number, B256::from([0x22; 32])),
                )
                .with_fork_choice_updated_v2_response(syncing_fcu())
                .with_fork_choice_updated_v3_response(syncing_fcu())
                .build(),
        );

        let mut state = TestEngineStateBuilder::new()
            .with_unsafe_head(test_block_info(100))
            .with_safe_head(L2BlockInfo::default())
            .with_finalized_head(L2BlockInfo::default())
            .with_el_sync_finished(false)
            .build();

        Engine::delegated_forkchoice_with_state(
            &mut state,
            client,
            Arc::new(RollupConfig::default()),
            DelegatedForkchoiceUpdate {
                safe_l2: delegated_safe,
                finalized_l2_number: Some(delegated_safe_number),
            },
        )
        .await
        .expect("delegated forkchoice should not fail");

        assert_eq!(
            state.sync_state.safe_head(),
            L2BlockInfo::default(),
            "safe head must remain unchanged when safe FCU returns Syncing",
        );
        assert_eq!(
            state.sync_state.finalized_head(),
            L2BlockInfo::default(),
            "finalized head must not advance past the actual safe head",
        );
    }
}
