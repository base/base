//! Direct safe-head consolidation operations.

use std::{sync::Arc, time::Instant};

use alloy_rpc_types_eth::Block;
use base_common_genesis::RollupConfig;
use base_common_rpc_types::Transaction;
use base_protocol::{AttributesWithParent, L2BlockInfo};
use thiserror::Error;

use crate::{
    BuildTaskError, Engine, EngineClient, EngineState, EngineSyncStateUpdate, EngineTaskError,
    EngineTaskErrorSeverity, InsertPayloadSafety, Metrics, SealTaskError, SynchronizeTask,
    SynchronizeTaskError,
};

/// An error that occurs when consolidating the engine state.
#[derive(Debug, Error)]
pub enum ConsolidateTaskError {
    /// The unsafe L2 block is missing.
    #[error("Unsafe L2 block is missing {0}")]
    MissingUnsafeL2Block(u64),
    /// Failed to fetch the unsafe L2 block.
    #[error("Failed to fetch the unsafe L2 block")]
    FailedToFetchUnsafeL2Block,
    /// The build task failed.
    #[error(transparent)]
    BuildTaskFailed(#[from] BuildTaskError),
    /// The seal task failed.
    #[error(transparent)]
    SealTaskFailed(#[from] SealTaskError),
    /// The consolidation forkchoice update call to the engine api failed.
    #[error(transparent)]
    ForkchoiceUpdateFailed(#[from] SynchronizeTaskError),
}

impl EngineTaskError for ConsolidateTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::MissingUnsafeL2Block(_) => EngineTaskErrorSeverity::Reset,
            Self::FailedToFetchUnsafeL2Block => EngineTaskErrorSeverity::Temporary,
            Self::BuildTaskFailed(inner) => inner.severity(),
            Self::SealTaskFailed(inner) => inner.severity(),
            Self::ForkchoiceUpdateFailed(inner) => inner.severity(),
        }
    }
}

/// Input for consolidation - either derived attributes or safe L2 block
#[derive(Debug, Clone)]
pub enum ConsolidateInput {
    /// Consolidate based on derived attributes.
    Attributes(Box<AttributesWithParent>),
    /// Derivation Delegation: consolidate based on safe L2 block info.
    BlockInfo(L2BlockInfo),
}

impl From<L2BlockInfo> for ConsolidateInput {
    fn from(v: L2BlockInfo) -> Self {
        Self::BlockInfo(v)
    }
}

impl From<AttributesWithParent> for ConsolidateInput {
    fn from(v: AttributesWithParent) -> Self {
        Self::Attributes(Box::new(v))
    }
}

impl ConsolidateInput {
    /// Returns the block number for this consolidation input.
    pub const fn l2_block_number(&self) -> u64 {
        match self {
            Self::Attributes(attributes) => attributes.block_number(),
            Self::BlockInfo(info) => info.block_info.number,
        }
    }

    /// Checks if the block is consistent with this consolidation input.
    pub fn is_consistent_with_block(&self, cfg: &RollupConfig, block: &Block<Transaction>) -> bool {
        match self {
            Self::Attributes(attributes) => {
                crate::AttributesMatch::check(cfg, attributes, block).is_match()
            }
            Self::BlockInfo(info) => block.header.hash == info.block_info.hash,
        }
    }

    /// Returns true if this is `Attributes` and `attributes.is_last_in_span` is true.
    pub const fn is_attributes_last_in_span(&self) -> bool {
        matches!(
            self,
            Self::Attributes(attributes)
                if attributes.is_last_in_span
        )
    }
}

impl Engine {
    /// Consolidates the safe head directly against the execution layer.
    pub async fn consolidate<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError>
    where
        EngineClient_: EngineClient + 'static,
    {
        self.retry_with_severity(Metrics::CONSOLIDATE_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            let input = input.clone();
            Box::pin(
                async move { Self::consolidate_with_state(state, client, config, input).await },
            )
        })
        .await
    }

    /// Consolidates the safe head using the provided engine state.
    pub async fn consolidate_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError> {
        // Behavior depends on how the safe head is provided:
        //
        // - `Attributes`: The safe head is advanced through the normal derivation flow, where the
        //   DerivationActor and EngineActor coordinate both safe and unsafe heads. In this case, we
        //   consolidate as long as the unsafe head has not fallen behind.
        //
        // - `BlockInfo`: The safe head is injected externally by the DerivationActor while
        //   delegating derivation, and is not coordinated with the EngineActor's safe/unsafe heads.
        //   If the injected safe head is ahead of the EngineActor's unsafe head, we reconcile the
        //   unsafe chain up to the safe head instead of consolidating.
        let safe_head_number = match &input {
            ConsolidateInput::Attributes { .. } => state.sync_state.safe_head().block_info.number,
            ConsolidateInput::BlockInfo(safe_block_info) => safe_block_info.block_info.number,
        };
        if safe_head_number < state.sync_state.unsafe_head().block_info.number {
            Self::consolidate_safe_head(state, client, config, input).await
        } else {
            Self::reconcile_unsafe_to_safe(state, client, config, &input).await
        }
    }

    /// Rebuilds and seals attributes when consolidation cannot use the current unsafe block.
    pub async fn rebuild_safe_payload<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        attributes: &AttributesWithParent,
    ) -> Result<(), ConsolidateTaskError> {
        let payload_id =
            Self::build_with_state(state, client.as_ref(), config.as_ref(), attributes.clone())
                .await?;

        Self::seal_started_payload_with_state(
            state,
            client,
            config,
            payload_id,
            attributes.clone(),
            InsertPayloadSafety::Safe,
        )
        .await?;

        Ok(())
    }

    /// Reconciles the engine unsafe, local safe, and safe heads to an externally supplied safe head.
    pub async fn reconcile_to_safe_head<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        safe_l2: &L2BlockInfo,
    ) -> Result<(), ConsolidateTaskError> {
        warn!(
            target: "engine",
            safe_l2 = %safe_l2,
            "Apply safe head"
        );

        let fcu_start = Instant::now();

        // We intentionally set unsafe_head to safe_l2 to ensure the engine observes a
        // self-consistent head state. This is required to correctly handle reorgs (where unsafe
        // may be ahead on a non-canonical fork) and to trigger EL sync when the local unsafe head
        // lags behind the safe head.
        SynchronizeTask::new(
            client,
            config,
            EngineSyncStateUpdate {
                unsafe_head: Some(*safe_l2),
                local_safe_head: Some(*safe_l2),
                safe_head: Some(*safe_l2),
                ..Default::default()
            },
        )
        .execute(state)
        .await
        .map_err(|e| {
            warn!(target: "engine", error = ?e, "Apply safe head failed");
            e
        })?;

        let fcu_duration = fcu_start.elapsed();

        info!(
            target: "engine",
            hash = %safe_l2.block_info.hash,
            number = safe_l2.block_info.number,
            fcu_duration = ?fcu_duration,
            "Updated safe head via follow safe"
        );

        Ok(())
    }

    /// Reconciles the unsafe chain to the safe input when direct consolidation cannot be used.
    pub async fn reconcile_unsafe_to_safe<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: &ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError> {
        match input {
            ConsolidateInput::Attributes(attributes) => {
                Self::rebuild_safe_payload(state, client, config, attributes).await
            }
            ConsolidateInput::BlockInfo(safe_l2) => {
                Self::reconcile_to_safe_head(state, client, config, safe_l2).await
            }
        }
    }

    /// Consolidates the safe head by checking the current unsafe block against the input.
    pub async fn consolidate_safe_head<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError> {
        let global_start = Instant::now();

        let block_num = input.l2_block_number();
        let fetch_start = Instant::now();
        let block = match client.l2_block_by_label(block_num.into()).await {
            Ok(Some(block)) => block,
            Ok(None) => {
                warn!(target: "engine", block_num, "Received `None` block");
                return Err(ConsolidateTaskError::MissingUnsafeL2Block(block_num));
            }
            Err(_) => {
                warn!(target: "engine", "Failed to fetch unsafe l2 block for consolidation");
                return Err(ConsolidateTaskError::FailedToFetchUnsafeL2Block);
            }
        };
        let block_fetch_duration = fetch_start.elapsed();
        let block_hash = block.header.hash;

        if input.is_consistent_with_block(&config, &block) {
            trace!(
                target: "engine",
                input = ?input,
                block_hash = %block_hash,
                "Consolidating engine state",
            );
            match L2BlockInfo::from_block_and_genesis(
                &block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
                &config.genesis,
            ) {
                // Only issue a forkchoice update if the attributes are the last in the span
                // batch. This is an optimization to avoid sending a FCU call for every block in
                // the span batch.
                Ok(block_info) if !input.is_attributes_last_in_span() => {
                    let total_duration = global_start.elapsed();

                    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
                        local_safe_head: Some(block_info),
                        safe_head: Some(block_info),
                        ..Default::default()
                    });

                    info!(
                        target: "engine",
                        hash = %block_info.block_info.hash,
                        number = block_info.block_info.number,
                        ?total_duration,
                        ?block_fetch_duration,
                        "Updated safe head via L1 consolidation"
                    );

                    return Ok(());
                }
                Ok(block_info) => {
                    let fcu_start = Instant::now();

                    SynchronizeTask::new(
                        Arc::clone(&client),
                        Arc::clone(&config),
                        EngineSyncStateUpdate {
                            local_safe_head: Some(block_info),
                            safe_head: Some(block_info),
                            ..Default::default()
                        },
                    )
                    .execute(state)
                    .await
                    .map_err(|e| {
                        warn!(target: "engine", error = ?e, "Consolidation failed");
                        e
                    })?;

                    let fcu_duration = fcu_start.elapsed();
                    let total_duration = global_start.elapsed();

                    info!(
                        target: "engine",
                        hash = %block_info.block_info.hash,
                        number = block_info.block_info.number,
                        ?total_duration,
                        ?block_fetch_duration,
                        fcu_duration = ?fcu_duration,
                        "Updated safe head via L1 consolidation"
                    );

                    return Ok(());
                }
                Err(e) => {
                    warn!(target: "engine", error = ?e, "Failed to construct L2BlockInfo, proceeding to safe payload rebuild");
                }
            }
        }

        debug!(
            target: "engine",
            input = ?input,
            block_hash = %block_hash,
            "ConsolidateInput mismatch! Initiating reorg",
        );
        Self::reconcile_unsafe_to_safe(state, client, config, &input).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::{BlockNumberOrTag, Encodable2718};
    use alloy_primitives::{Address, FixedBytes, b256};
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum};
    use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_rpc_types::Transaction as BaseTransaction;
    use base_protocol::L1BlockInfoBedrock;

    use super::*;
    use crate::{
        AttributesMatch, AttributesMismatch,
        test_utils::{TestAttributesBuilder, TestEngineStateBuilder, test_engine_client_builder},
    };

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

    #[tokio::test]
    async fn consolidate_does_not_crash_when_safe_behind_unsafe_and_attributes_mismatch() {
        let safe_head = crate::test_utils::test_block_info(34);
        let unsafe_head = crate::test_utils::test_block_info(76);

        let attributes =
            TestAttributesBuilder::new().with_parent(safe_head).with_timestamp(2000).build();

        let mut state = TestEngineStateBuilder::new()
            .with_unsafe_head(unsafe_head)
            .with_safe_head(safe_head)
            .with_finalized_head(safe_head)
            .build();

        let mut mismatched_block = RpcBlock::<BaseTransaction>::default();
        mismatched_block.header.inner.number = 35;
        mismatched_block.header.inner.timestamp = 2000;
        mismatched_block.header.inner.parent_hash =
            b256!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

        let valid_fcu = ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: Some(FixedBytes([2u8; 32])),
            },
            payload_id: Some(PayloadId::new([1u8; 8])),
        };
        let client = Arc::new(
            test_engine_client_builder()
                .with_l2_block_by_label(BlockNumberOrTag::Number(35), mismatched_block)
                .with_fork_choice_updated_v2_response(valid_fcu.clone())
                .with_fork_choice_updated_v3_response(valid_fcu)
                .build(),
        );

        let result = Engine::consolidate_with_state(
            &mut state,
            client,
            Arc::new(RollupConfig::default()),
            ConsolidateInput::from(attributes),
        )
        .await;

        if let Err(ref err) = result {
            let err_msg = format!("{err}");
            assert!(
                !err_msg.contains("Unsafe head changed between build and seal"),
                "must not fail with UnsafeHeadChangedSinceBuild: {err}",
            );
        }
    }

    #[tokio::test]
    async fn consolidate_rejects_attribute_transaction_with_trailing_bytes() {
        let safe_head = crate::test_utils::test_block_info(0);
        let tx = l1_info_deposit_tx();
        let mut attr_tx = Vec::new();
        tx.encode_2718(&mut attr_tx);
        attr_tx.extend_from_slice(b"trailing bytes");

        let attributes = TestAttributesBuilder::new()
            .with_parent(safe_head)
            .with_transactions(vec![attr_tx.into()])
            .build();
        let block_number = attributes.block_number();

        let mut unsafe_block = RpcBlock::<BaseTransaction>::default();
        unsafe_block.header.inner.number = block_number;
        unsafe_block.header.inner.parent_hash = safe_head.block_info.hash;
        unsafe_block.header.inner.timestamp = attributes.attributes().payload_attributes.timestamp;
        unsafe_block.header.inner.mix_hash = attributes.attributes().payload_attributes.prev_randao;
        unsafe_block.header.inner.gas_limit = attributes.attributes().gas_limit.unwrap_or_default();
        unsafe_block.header.inner.parent_beacon_block_root =
            attributes.attributes().payload_attributes.parent_beacon_block_root;
        unsafe_block.transactions =
            BlockTransactions::Full(vec![rpc_transaction(tx, block_number)]);

        let cfg = RollupConfig::default();
        assert_eq!(
            AttributesMatch::check(&cfg, &attributes, &unsafe_block),
            AttributesMismatch::MalformedAttributesTransaction.into(),
        );

        let mut state = TestEngineStateBuilder::new()
            .with_safe_head(safe_head)
            .with_unsafe_head(crate::test_utils::test_block_info(block_number))
            .build();
        let original_safe_head = state.sync_state.safe_head();
        let original_local_safe_head = state.sync_state.local_safe_head();
        let client = Arc::new(
            test_engine_client_builder()
                .with_l2_block_by_label(BlockNumberOrTag::Number(block_number), unsafe_block)
                .build(),
        );
        let result = Engine::consolidate_with_state(
            &mut state,
            client,
            Arc::new(cfg),
            ConsolidateInput::from(attributes),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(state.sync_state.safe_head(), original_safe_head);
        assert_eq!(state.sync_state.local_safe_head(), original_local_safe_head);
    }
}
