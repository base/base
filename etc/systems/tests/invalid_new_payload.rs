//! System-style regression test for EL-invalid unsafe payload handling.

use std::{sync::Arc, time::Duration};

use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bloom, U256};
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_consensus_derive::Signal;
use base_consensus_engine::{
    Engine,
    test_utils::{MockEngineClient, TestEngineStateBuilder, test_engine_client_builder},
};
use base_consensus_node::{
    DerivationClientResult, EngineActorRequest, EngineError, EngineProcessor,
    EngineRequestReceiver, NoopCheckpointWriter, ValidatorEngineRequestHandler,
};
use base_protocol::{L1BlockInfoBedrock, L2BlockInfo};
use eyre::{Result, WrapErr};
use tokio::{
    sync::{mpsc, watch},
    time::{sleep, timeout},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct NoopDerivationClient;

#[async_trait::async_trait]
impl base_consensus_node::EngineDerivationClient for NoopDerivationClient {
    async fn notify_sync_completed(&self, _safe_head: L2BlockInfo) -> DerivationClientResult<()> {
        Ok(())
    }

    async fn send_new_engine_safe_head(
        &self,
        _safe_head: L2BlockInfo,
    ) -> DerivationClientResult<()> {
        Ok(())
    }

    async fn send_signal(&self, _signal: Signal) -> DerivationClientResult<()> {
        Ok(())
    }
}

/// An EL-invalid unsafe payload must be consumed so the engine actor can accept an alternative
/// payload at the same height. Before the fix, the first payload was retried indefinitely and the
/// actor never received the following request.
#[tokio::test]
async fn el_invalid_unsafe_payload_does_not_block_following_payload() -> Result<()> {
    let config = Arc::new(RollupConfig::default());
    let initial_head = l2_block_info(0, B256::with_last_byte(0xee), B256::ZERO);
    let client = Arc::new(
        test_engine_client_builder()
            .with_config(Arc::clone(&config))
            .with_block_info_by_tag(BlockNumberOrTag::Latest, initial_head)
            .with_new_payload_v2_response(invalid_payload_status())
            .with_fork_choice_updated_v3_response(valid_forkchoice_updated())
            .build(),
    );
    let initial_state = TestEngineStateBuilder::new()
        .with_unsafe_head(initial_head)
        .with_safe_head(initial_head)
        .with_finalized_head(initial_head)
        .with_el_sync_finished(true)
        .build();
    let (state_tx, state_rx) = watch::channel(initial_state);
    let (queue_tx, _) = watch::channel(0usize);
    let processor = EngineProcessor::new_with_checkpoint(
        Arc::clone(&client),
        Arc::clone(&config),
        NoopDerivationClient,
        Engine::new(initial_state, state_tx, queue_tx),
        Arc::new(base_consensus_engine::NoopForkchoiceCheckpointReader),
        Arc::new(NoopCheckpointWriter),
    );
    let (request_tx, request_rx) = mpsc::channel(8);
    let mut handle = ValidatorEngineRequestHandler::new(processor).start(request_rx);

    request_tx
        .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(unsafe_payload(
            initial_head.block_info.hash,
            1,
        ))))
        .await?;

    wait_for_new_payload_attempt(&client).await?;
    client.set_new_payload_v2_response(valid_payload_status()).await;

    let valid_payload = unsafe_payload(initial_head.block_info.hash, 2);
    let expected_hash = L2BlockInfo::from_payload_and_genesis(
        valid_payload.execution_payload.clone(),
        valid_payload.parent_beacon_block_root,
        &config.genesis,
    )?
    .block_info
    .hash;
    request_tx
        .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(valid_payload)))
        .await?;

    let wanted_hash = expected_hash;
    let mut state_updates = state_rx.clone();
    let state_update = timeout(
        REQUEST_TIMEOUT,
        state_updates
            .wait_for(move |state| state.sync_state.unsafe_head().block_info.hash == wanted_hash),
    )
    .await;
    match state_update {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            let result = handle.await.wrap_err("engine processor task panicked")?;
            return Err(eyre::eyre!(
                "engine state channel closed before following unsafe payload was inserted: {error:?}; engine result: {result:?}"
            ));
        }
        Err(_) => {
            return Err(eyre::eyre!(
                "following unsafe payload was blocked by an EL-invalid payload; state: {:?}; engine finished: {}",
                *state_rx.borrow(),
                handle.is_finished(),
            ));
        }
    }

    drop(request_tx);
    let result = timeout(REQUEST_TIMEOUT, &mut handle)
        .await
        .wrap_err("engine processor did not stop after its request channel closed")?
        .wrap_err("engine processor task panicked")?;
    assert!(
        matches!(result, Err(EngineError::ChannelClosed)),
        "expected a clean ChannelClosed shutdown, got {result:?}"
    );

    Ok(())
}

async fn wait_for_new_payload_attempt(client: &MockEngineClient) -> Result<()> {
    timeout(REQUEST_TIMEOUT, async {
        loop {
            if client.last_new_payload_v2().await.is_some() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .wrap_err("engine never attempted the EL-invalid payload")?;
    Ok(())
}

fn invalid_payload_status() -> PayloadStatus {
    PayloadStatus {
        status: PayloadStatusEnum::Invalid { validation_error: "invalid transaction".into() },
        latest_valid_hash: None,
    }
}

const fn valid_payload_status() -> PayloadStatus {
    PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }
}

const fn valid_forkchoice_updated() -> ForkchoiceUpdated {
    ForkchoiceUpdated { payload_status: valid_payload_status(), payload_id: None }
}

fn l2_block_info(number: u64, hash: B256, parent_hash: B256) -> L2BlockInfo {
    L2BlockInfo {
        block_info: base_protocol::BlockInfo { number, hash, parent_hash, timestamp: number },
        ..Default::default()
    }
}

fn unsafe_payload(parent_hash: B256, marker: u8) -> BaseExecutionPayloadEnvelope {
    BaseExecutionPayloadEnvelope {
        parent_beacon_block_root: None,
        execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: Address::ZERO,
            state_root: B256::with_last_byte(marker),
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number: 1,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: 1,
            extra_data: Default::default(),
            base_fee_per_gas: U256::ZERO,
            block_hash: B256::with_last_byte(marker),
            transactions: vec![l1_info_deposit_tx().into()],
        }),
    }
}

fn l1_info_deposit_tx() -> Vec<u8> {
    BaseTxEnvelope::from(TxDeposit {
        input: L1BlockInfoBedrock::default().encode_calldata(),
        ..Default::default()
    })
    .encoded_2718()
}
