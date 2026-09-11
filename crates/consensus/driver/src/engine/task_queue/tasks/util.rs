//! Derivation building and canonicalization with a bounded deposits-only fallback.
use std::sync::Arc;

use base_common_chain_config::RollupConfig;
use base_consensus_batch::AttributesWithParent;

use crate::engine::{
    BuildTaskError, Engine, EngineClient, EngineState, EngineTaskExt, InsertPayloadSafety,
    InsertTask, InsertTaskError, SealTaskError,
};

/// Failure building or appending a derived block.
#[derive(Debug, thiserror::Error)]
pub enum BuildAndAppendError {
    /// Starting the build failed.
    #[error(transparent)]
    Build(#[from] BuildTaskError),
    /// Resolving or appending the build failed.
    #[error(transparent)]
    Seal(#[from] SealTaskError),
}

/// Builds and appends derived blocks, retrying invalid transactions once without sequencer transactions.
#[derive(Debug)]
pub struct BuildAndAppend;
impl BuildAndAppend {
    /// Builds and appends a derived block, preserving the successful fallback flush signal.
    pub async fn execute<E: EngineClient>(
        state: &mut EngineState,
        engine: Arc<E>,
        cfg: Arc<RollupConfig>,
        attributes: AttributesWithParent,
        safety: InsertPayloadSafety,
    ) -> Result<(), BuildAndAppendError> {
        match Self::attempt(state, &engine, &cfg, attributes.clone(), safety).await {
            Err(BuildAndAppendError::Seal(SealTaskError::PayloadInsertionFailed(error)))
                if matches!(*error, InsertTaskError::UnexpectedPayloadStatus(_)) =>
            {
                if attributes.is_deposits_only() {
                    return Err(SealTaskError::DepositOnlyPayloadFailed.into());
                }
                match Self::attempt(state, &engine, &cfg, attributes.as_deposits_only(), safety)
                    .await
                {
                    Ok(()) => Err(SealTaskError::HoloceneInvalidFlush.into()),
                    Err(_) => Err(SealTaskError::DepositOnlyPayloadReattemptFailed.into()),
                }
            }
            result => result,
        }
    }

    /// Executes one build and append attempt.
    pub async fn attempt<E: EngineClient>(
        state: &mut EngineState,
        engine: &Arc<E>,
        cfg: &Arc<RollupConfig>,
        attributes: AttributesWithParent,
        safety: InsertPayloadSafety,
    ) -> Result<(), BuildAndAppendError> {
        let id = Engine::<E>::start_building_with_state(state, engine.as_ref(), attributes).await?;
        let payload = engine.end_building(id).await.map_err(SealTaskError::GetPayloadFailed)?;
        InsertTask::new(Arc::clone(engine), Arc::clone(cfg), payload, safety)
            .execute(state)
            .await
            .map_err(|error| SealTaskError::PayloadInsertionFailed(Box::new(error)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use base_common_types_payload::{
        BaseExecutionPayload, BaseExecutionPayloadEnvelope, ExecutionPayloadV1, ForkchoiceUpdated,
        PayloadId, PayloadStatus, PayloadStatusEnum,
    };

    use super::*;
    use crate::{
        engine_test_utils::TestAttributesBuilder,
        test_utils::{FakeEngineClient, ScriptedForkchoiceResponse},
    };

    #[tokio::test]
    async fn invalid_payload_fallback_is_bounded_and_flushes_only_after_success() {
        for (deposits_only, retry_valid) in [(false, true), (false, false), (true, false)] {
            let mut payload = ExecutionPayloadV1 {
                parent_hash: Default::default(),
                fee_recipient: Default::default(),
                state_root: Default::default(),
                receipts_root: Default::default(),
                logs_bloom: Default::default(),
                prev_randao: Default::default(),
                block_number: 0,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 0,
                extra_data: Default::default(),
                base_fee_per_gas: Default::default(),
                block_hash: Default::default(),
                transactions: vec![],
            };
            let genesis_block: base_common_types_chain::BaseBlock =
                BaseExecutionPayload::V1(payload.clone()).try_into_block().unwrap();
            payload.block_hash = genesis_block.header.hash_slow();
            let mut cfg = RollupConfig::default();
            cfg.genesis.l2.hash = payload.block_hash;
            let cfg = Arc::new(cfg);
            let client = Arc::new(FakeEngineClient::new(Arc::clone(&cfg)).with_built_payload(Ok(
                BaseExecutionPayloadEnvelope {
                    execution_payload: BaseExecutionPayload::V1(payload),
                    parent_beacon_block_root: None,
                },
            )));
            let handle = client.handle();
            let valid = PayloadStatus::from_status(PayloadStatusEnum::Valid);
            let invalid = PayloadStatus::from_status(PayloadStatusEnum::Invalid {
                validation_error: "execution failed".into(),
            });
            handle.push_scripted_payload([
                invalid.clone(),
                if retry_valid { valid.clone() } else { invalid },
            ]);
            let build = ForkchoiceUpdated {
                payload_status: valid.clone(),
                payload_id: Some(PayloadId::new([1; 8])),
            };
            handle.push_scripted_forkchoice([
                ScriptedForkchoiceResponse::Ok(build.clone()),
                ScriptedForkchoiceResponse::Ok(build),
                ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated::new(valid)),
            ]);
            let mut attributes = TestAttributesBuilder::new();
            if !deposits_only {
                attributes = attributes.with_transactions(vec![Bytes::from_static(&[2])]);
            }
            let error = BuildAndAppend::execute(
                &mut EngineState::default(),
                client,
                cfg,
                attributes.build(),
                InsertPayloadSafety::Safe,
            )
            .await
            .unwrap_err();
            match (deposits_only, retry_valid, error) {
                (false, true, BuildAndAppendError::Seal(SealTaskError::HoloceneInvalidFlush))
                | (
                    false,
                    false,
                    BuildAndAppendError::Seal(SealTaskError::DepositOnlyPayloadReattemptFailed),
                )
                | (true, _, BuildAndAppendError::Seal(SealTaskError::DepositOnlyPayloadFailed)) => {
                }
                (_, _, error) => panic!("unexpected fallback result: {error}"),
            }
        }
    }
}
