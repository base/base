//! Direct payload fetch and seal operations.

use std::{sync::Arc, time::Instant};

use alloy_rpc_types_engine::{ExecutionPayload, PayloadId};
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_protocol::{AttributesWithParent, FromBlockError, L2BlockInfo};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    Engine, EngineClient, EngineGetPayloadVersion, EngineState, EngineTaskError,
    EngineTaskErrorSeverity, InsertPayloadSafety, InsertTaskError, Metrics, SynchronizeTaskError,
};

/// An error that occurs when sealing a started payload.
#[derive(Debug, Error)]
pub enum SealTaskError {
    /// Impossible to insert the payload into the engine.
    #[error(transparent)]
    PayloadInsertionFailed(#[from] Box<InsertTaskError>),
    /// The get payload call to the engine api failed.
    #[error(transparent)]
    GetPayloadFailed(RpcError<TransportErrorKind>),
    /// A deposit-only payload failed to import.
    #[error("Deposit-only payload failed to import")]
    DepositOnlyPayloadFailed,
    /// Failed to re-attempt payload import with deposit-only payload.
    #[error("Failed to re-attempt payload import with deposit-only payload: {0}")]
    DepositOnlyPayloadReattemptFailed(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The payload is invalid, and the derivation pipeline must
    /// be flushed post-holocene.
    #[error("Invalid payload, must flush post-holocene")]
    HoloceneInvalidFlush,
    /// Failed to convert a [`BaseExecutionPayload`] to a [`L2BlockInfo`].
    ///
    /// [`BaseExecutionPayload`]: base_common_rpc_types_engine::BaseExecutionPayload
    /// [`L2BlockInfo`]: base_protocol::L2BlockInfo
    #[error(transparent)]
    FromBlock(#[from] FromBlockError),
    /// Error sending the built payload envelope.
    #[error(transparent)]
    MpscSend(#[from] Box<mpsc::error::SendError<Result<BaseExecutionPayloadEnvelope, Self>>>),
    /// The clock went backwards.
    #[error("The clock went backwards")]
    ClockWentBackwards,
    /// Unsafe head changed between build and seal. This likely means that there was some race
    /// condition between the previous seal updating the unsafe head and the build attributes
    /// being created. This build has been invalidated.
    ///
    /// If not propagated to the original caller for handling (i.e. there was no original caller),
    /// this should not happen and is a critical error.
    #[error("Unsafe head changed between build and seal")]
    UnsafeHeadChangedSinceBuild,
    /// The execution layer returned a payload version that does not match the requested
    /// get-payload method.
    #[error("Unexpected payload version from get_payload: {0}")]
    UnexpectedPayloadVersion(String),
}

impl SealTaskError {
    /// Whether this error is fatal from the sequencer's perspective.
    ///
    /// This classification is intentionally separate from [`EngineTaskError::severity`] because
    /// the sequencer may interpret error severity differently than the engine. The exhaustive
    /// match ensures new variants cause a compile error until explicitly classified here.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::PayloadInsertionFailed(insert_err) => match &**insert_err {
                InsertTaskError::ForkchoiceUpdateFailed(synchronize_error) => {
                    match synchronize_error {
                        SynchronizeTaskError::FinalizedAheadOfUnsafe(_, _) => true,
                        SynchronizeTaskError::ForkchoiceUpdateFailed(_)
                        | SynchronizeTaskError::InvalidForkchoiceState
                        | SynchronizeTaskError::UnexpectedPayloadStatus(_) => false,
                    }
                }
                InsertTaskError::FromBlockError(_)
                | InsertTaskError::L2BlockInfoConstruction(_) => true,
                InsertTaskError::InsertFailed(_)
                | InsertTaskError::UnexpectedPayloadStatus(_)
                | InsertTaskError::ForkchoiceUpdateDidNotAdvance => false,
            },
            Self::GetPayloadFailed(_)
            | Self::HoloceneInvalidFlush
            | Self::UnsafeHeadChangedSinceBuild
            | Self::UnexpectedPayloadVersion(_) => false,
            Self::DepositOnlyPayloadFailed
            | Self::DepositOnlyPayloadReattemptFailed(_)
            | Self::FromBlock(_)
            | Self::MpscSend(_)
            | Self::ClockWentBackwards => true,
        }
    }
}

impl EngineTaskError for SealTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::PayloadInsertionFailed(inner) => inner.severity(),
            Self::GetPayloadFailed(_) | Self::UnexpectedPayloadVersion(_) => {
                EngineTaskErrorSeverity::Temporary
            }
            Self::HoloceneInvalidFlush => EngineTaskErrorSeverity::Flush,
            Self::UnsafeHeadChangedSinceBuild => EngineTaskErrorSeverity::Reset,
            Self::DepositOnlyPayloadReattemptFailed(_)
            | Self::DepositOnlyPayloadFailed
            | Self::FromBlock(_)
            | Self::MpscSend(_)
            | Self::ClockWentBackwards => EngineTaskErrorSeverity::Critical,
        }
    }
}

impl Engine {
    /// Fetches a sealed payload from the execution layer without inserting it.
    pub async fn get_payload<EngineClient_: EngineClient>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, SealTaskError> {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::GET_PAYLOAD_TASK_LABEL));

        let result = Self::get_payload_with_state(
            &self.state,
            client.as_ref(),
            config.as_ref(),
            payload_id,
            &attributes,
        )
        .await;

        match result {
            Ok(envelope) => {
                Metrics::engine_task_count(Metrics::GET_PAYLOAD_TASK_LABEL).increment(1);
                Ok(envelope)
            }
            Err(err) => {
                Metrics::engine_task_failure(
                    Metrics::GET_PAYLOAD_TASK_LABEL,
                    err.severity().as_label(),
                )
                .increment(1);
                Err(err)
            }
        }
    }

    /// Fetches a sealed payload using the provided engine state.
    pub async fn get_payload_with_state<EngineClient_: EngineClient>(
        state: &EngineState,
        engine: &EngineClient_,
        cfg: &RollupConfig,
        payload_id: PayloadId,
        payload_attrs: &AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, SealTaskError> {
        debug!(
            target: "engine",
            "Starting new get-payload job"
        );

        let unsafe_block_info = state.sync_state.unsafe_head().block_info;
        let parent_block_info = payload_attrs.parent.block_info;

        if unsafe_block_info.hash != parent_block_info.hash
            || unsafe_block_info.number != parent_block_info.number
        {
            error!(
                target: "engine",
                unsafe_block_info = ?unsafe_block_info,
                parent_block_info = ?parent_block_info,
                "GetPayload attributes parent does not match unsafe head, returning rebuild error"
            );
            Metrics::sequencer_unsafe_head_changed_total().increment(1);
            return Err(SealTaskError::UnsafeHeadChangedSinceBuild);
        }

        Self::fetch_payload(cfg, engine, payload_id, payload_attrs).await
    }

    /// Fetches the payload from the execution layer using the payload timestamp for versioning.
    pub async fn fetch_payload<EngineClient_: EngineClient>(
        cfg: &RollupConfig,
        engine: &EngineClient_,
        payload_id: PayloadId,
        payload_attrs: &AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, SealTaskError> {
        let payload_timestamp = payload_attrs.attributes().payload_attributes.timestamp;

        debug!(
            target: "engine",
            payload_id = payload_id.to_string(),
            l2_time = payload_timestamp,
            "Fetching payload"
        );

        let get_payload_version = EngineGetPayloadVersion::from_cfg(cfg, payload_timestamp);
        let payload_envelope = match get_payload_version {
            EngineGetPayloadVersion::V5 => {
                let payload = engine.get_payload_v5(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: payload_attrs
                        .attributes()
                        .payload_attributes
                        .parent_beacon_block_root,
                    execution_payload: BaseExecutionPayload::V4(payload.execution_payload),
                }
            }
            EngineGetPayloadVersion::V4 => {
                let payload = engine.get_payload_v4(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: Some(payload.parent_beacon_block_root),
                    execution_payload: BaseExecutionPayload::V4(payload.execution_payload),
                }
            }
            EngineGetPayloadVersion::V3 => {
                let payload = engine.get_payload_v3(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: Some(payload.parent_beacon_block_root),
                    execution_payload: BaseExecutionPayload::V3(payload.execution_payload),
                }
            }
            EngineGetPayloadVersion::V2 => {
                let payload = engine.get_payload_v2(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: None,
                    execution_payload: match payload.execution_payload.into_payload() {
                        ExecutionPayload::V1(payload) => BaseExecutionPayload::V1(payload),
                        ExecutionPayload::V2(payload) => BaseExecutionPayload::V2(payload),
                        other => {
                            return Err(SealTaskError::UnexpectedPayloadVersion(format!(
                                "{other:?}"
                            )));
                        }
                    },
                }
            }
        };

        Ok(payload_envelope)
    }

    /// Fetches a started payload from the execution layer and imports it into forkchoice.
    pub async fn seal_started_payload_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
        payload_safety: InsertPayloadSafety,
    ) -> Result<BaseExecutionPayloadEnvelope, SealTaskError> {
        debug!(
            target: "engine",
            txs = attributes.attributes().transactions.as_ref().map_or(0, |txs| txs.len()),
            is_deposits = attributes.is_deposits_only(),
            "Starting payload seal"
        );

        let block_import_start_time = Instant::now();
        let payload =
            Self::fetch_payload(config.as_ref(), client.as_ref(), payload_id, &attributes).await?;

        let new_block_ref = L2BlockInfo::from_payload_header_and_genesis(
            &payload.execution_payload,
            &config.genesis,
        )
        .map_err(SealTaskError::FromBlock)?;

        Self::insert_sealed_payload_with_state(
            state,
            Arc::clone(&client),
            Arc::clone(&config),
            payload.clone(),
            &attributes,
            payload_safety,
        )
        .await?;

        let block_import_duration = block_import_start_time.elapsed();

        info!(
            target: "engine",
            l2_number = new_block_ref.block_info.number,
            l2_time = new_block_ref.block_info.timestamp,
            payload_safety = payload_safety.as_label(),
            block_import_duration = ?block_import_duration,
            "Built and imported new block",
        );

        Ok(payload)
    }

    /// Imports a sealed payload and applies the Holocene deposits-only fallback when needed.
    pub async fn insert_sealed_payload_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        payload: BaseExecutionPayloadEnvelope,
        attributes: &AttributesWithParent,
        payload_safety: InsertPayloadSafety,
    ) -> Result<(), SealTaskError> {
        match Self::insert_payload_with_state(
            state,
            Arc::clone(&client),
            Arc::clone(&config),
            payload,
            payload_safety,
        )
        .await
        {
            Err(InsertTaskError::UnexpectedPayloadStatus(err)) if attributes.is_deposits_only() => {
                error!(
                    target: "engine",
                    error = ?err,
                    "Critical: Deposit-only payload import failed"
                );
                Err(SealTaskError::DepositOnlyPayloadFailed)
            }
            Err(InsertTaskError::UnexpectedPayloadStatus(err))
                if config
                    .is_holocene_active(attributes.attributes().payload_attributes.timestamp) =>
            {
                warn!(
                    target: "engine",
                    error = ?err,
                    "Re-attempting payload import with deposits only"
                );

                let deposits_only_attributes = attributes.as_deposits_only();
                let payload_id = match Self::build_with_state(
                    state,
                    client.as_ref(),
                    config.as_ref(),
                    deposits_only_attributes.clone(),
                )
                .await
                {
                    Ok(payload_id) => payload_id,
                    Err(err) => {
                        error!(
                            target: "engine",
                            error = %err,
                            "Deposit-only build reattempt failed"
                        );
                        return Err(SealTaskError::DepositOnlyPayloadReattemptFailed(Box::new(
                            err,
                        )));
                    }
                };

                match Box::pin(Self::seal_started_payload_with_state(
                    state,
                    client,
                    config,
                    payload_id,
                    deposits_only_attributes,
                    payload_safety,
                ))
                .await
                {
                    Ok(_) => {
                        info!(
                            target: "engine",
                            "Successfully imported deposits-only payload"
                        );
                        Err(SealTaskError::HoloceneInvalidFlush)
                    }
                    Err(err) => {
                        error!(
                            target: "engine",
                            error = %err,
                            "Deposit-only seal reattempt failed"
                        );
                        Err(SealTaskError::DepositOnlyPayloadReattemptFailed(Box::new(err)))
                    }
                }
            }
            Err(err) => {
                error!(
                    target: "engine",
                    error = %err,
                    payload_safety = payload_safety.as_label(),
                    "Payload import failed"
                );
                Err(Box::new(err).into())
            }
            Ok(_) => {
                info!(
                    target: "engine",
                    payload_safety = payload_safety.as_label(),
                    "Successfully imported payload"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::{
        BlobsBundleV1, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
        ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum,
    };
    use alloy_transport::RpcError;
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{
        BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadEnvelopeV3,
    };
    use base_protocol::{FromBlockError, L1BlockInfoBedrock, L2BlockInfo};
    use rstest::rstest;

    use super::*;
    use crate::test_utils::{
        TestAttributesBuilder, TestEngineStateBuilder, test_block_info, test_engine_client_builder,
    };

    fn rpc_error() -> RpcError<TransportErrorKind> {
        RpcError::local_usage_str("test")
    }

    fn valid_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: Some(B256::ZERO),
            },
            payload_id: None,
        }
    }

    fn valid_fcu_with_payload(payload_id: PayloadId) -> ForkchoiceUpdated {
        ForkchoiceUpdated { payload_id: Some(payload_id), ..valid_fcu() }
    }

    fn valid_payload_status() -> PayloadStatus {
        PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(B256::ZERO) }
    }

    fn invalid_payload_status() -> PayloadStatus {
        PayloadStatus {
            status: PayloadStatusEnum::Invalid { validation_error: "invalid block".to_string() },
            latest_valid_hash: Some(B256::ZERO),
        }
    }

    fn l1_info_deposit_tx() -> Vec<u8> {
        BaseTxEnvelope::from(TxDeposit {
            input: L1BlockInfoBedrock::default().encode_calldata(),
            ..Default::default()
        })
        .encoded_2718()
    }

    fn execution_payload_v1(
        block_number: u64,
        parent_hash: B256,
        block_hash: B256,
        timestamp: u64,
    ) -> ExecutionPayloadV1 {
        ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp,
            extra_data: Default::default(),
            base_fee_per_gas: U256::ZERO,
            block_hash,
            transactions: vec![l1_info_deposit_tx().into()],
        }
    }

    fn execution_payload_v3(payload_inner: ExecutionPayloadV1) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 { payload_inner, withdrawals: Vec::new() },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    #[rstest]
    #[case::get_payload_failed(SealTaskError::GetPayloadFailed(rpc_error()), false)]
    #[case::unexpected_payload_version(
        SealTaskError::UnexpectedPayloadVersion("V3".to_string()),
        false
    )]
    #[case::holocene_invalid_flush(SealTaskError::HoloceneInvalidFlush, false)]
    #[case::unsafe_head_changed(SealTaskError::UnsafeHeadChangedSinceBuild, false)]
    #[case::deposit_only_failed(SealTaskError::DepositOnlyPayloadFailed, true)]
    #[case::deposit_only_reattempt_failed(
        SealTaskError::DepositOnlyPayloadReattemptFailed(Box::new(
            FromBlockError::InvalidGenesisHash,
        )),
        true
    )]
    #[case::from_block(SealTaskError::FromBlock(FromBlockError::InvalidGenesisHash), true)]
    #[case::clock_went_backwards(SealTaskError::ClockWentBackwards, true)]
    fn test_seal_task_error_is_fatal(#[case] err: SealTaskError, #[case] expected: bool) {
        assert_eq!(err.is_fatal(), expected);
    }

    #[rstest]
    #[case::finalized_ahead_of_unsafe(SynchronizeTaskError::FinalizedAheadOfUnsafe(10, 5), true)]
    #[case::forkchoice_update_failed(
        SynchronizeTaskError::ForkchoiceUpdateFailed(rpc_error()),
        false
    )]
    #[case::invalid_forkchoice_state(SynchronizeTaskError::InvalidForkchoiceState, false)]
    #[case::unexpected_payload_status(
        SynchronizeTaskError::UnexpectedPayloadStatus(PayloadStatusEnum::Invalid {
            validation_error: String::new(),
        }),
        false
    )]
    fn test_insertion_forkchoice_error_is_fatal(
        #[case] sync_err: SynchronizeTaskError,
        #[case] expected: bool,
    ) {
        let err = SealTaskError::PayloadInsertionFailed(Box::new(
            InsertTaskError::ForkchoiceUpdateFailed(sync_err),
        ));
        assert_eq!(err.is_fatal(), expected);
    }

    #[rstest]
    #[case::insert_failed(InsertTaskError::InsertFailed(rpc_error()), false)]
    #[case::unexpected_status(
        InsertTaskError::UnexpectedPayloadStatus(PayloadStatusEnum::Invalid {
            validation_error: String::new(),
        }),
        false
    )]
    #[case::l2_block_info_construction(
        InsertTaskError::L2BlockInfoConstruction(FromBlockError::InvalidGenesisHash),
        true
    )]
    fn test_insertion_non_forkchoice_error_is_fatal(
        #[case] insert_err: InsertTaskError,
        #[case] expected: bool,
    ) {
        let err = SealTaskError::PayloadInsertionFailed(Box::new(insert_err));
        assert_eq!(err.is_fatal(), expected);
    }

    #[tokio::test]
    async fn holocene_invalid_payload_flush_preserves_deposits_only_state() {
        let parent = test_block_info(0);
        let timestamp = 2_000;
        let mut cfg = RollupConfig::default();
        cfg.upgrades.holocene_time = Some(timestamp);
        let cfg = Arc::new(cfg);

        let attributes = TestAttributesBuilder::new()
            .with_parent(parent)
            .with_timestamp(timestamp)
            .with_transactions(vec![l1_info_deposit_tx().into(), vec![0x02, 0x00, 0x01].into()])
            .build();
        assert!(!attributes.is_deposits_only());

        let full_payload =
            execution_payload_v1(1, parent.block_info.hash, B256::with_last_byte(1), timestamp);
        let fallback_payload = execution_payload_v3(execution_payload_v1(
            1,
            parent.block_info.hash,
            B256::with_last_byte(2),
            timestamp,
        ));
        let expected_payload = BaseExecutionPayload::V3(fallback_payload.clone());
        let expected_unsafe_head =
            L2BlockInfo::from_payload_header_and_genesis(&expected_payload, &cfg.genesis)
                .expect("fallback payload should convert to L2BlockInfo");

        let payload_id = PayloadId::new([0x11; 8]);
        let client = Arc::new(
            test_engine_client_builder()
                .with_new_payload_v2_response(invalid_payload_status())
                .with_new_payload_v3_response(valid_payload_status())
                .with_fork_choice_updated_v3_response(valid_fcu_with_payload(payload_id))
                .with_execution_payload_v3(BaseExecutionPayloadEnvelopeV3 {
                    execution_payload: fallback_payload,
                    block_value: U256::ZERO,
                    blobs_bundle: BlobsBundleV1 {
                        commitments: Vec::new(),
                        proofs: Vec::new(),
                        blobs: Vec::new(),
                    },
                    should_override_builder: false,
                    parent_beacon_block_root: B256::ZERO,
                })
                .build(),
        );

        let mut state = TestEngineStateBuilder::new()
            .with_unsafe_head(parent)
            .with_safe_head(parent)
            .with_finalized_head(parent)
            .build();
        let payload = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(full_payload),
        };

        let err = Engine::insert_sealed_payload_with_state(
            &mut state,
            client,
            cfg,
            payload,
            &attributes,
            InsertPayloadSafety::Unsafe,
        )
        .await
        .expect_err("Holocene deposits-only reattempt should still flush derivation");

        assert!(matches!(err, SealTaskError::HoloceneInvalidFlush));
        assert_eq!(state.sync_state.unsafe_head(), expected_unsafe_head);
    }
}
