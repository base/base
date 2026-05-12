//! Integration tests for the engine processing path.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::Block as RpcBlock;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types::Transaction as BaseTransaction;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_engine::{
    DelegatedForkchoiceUpdate, Engine, EngineQueries,
    test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder},
};
use base_consensus_node::{
    BuildRequest, EngineActor, EngineActorRequest, EngineDerivationClient, EngineError,
    EngineProcessingRequest, EngineProcessor, EngineProcessorOptions, EngineRequestReceiver,
    NodeActor, NodeMode, QueuedEngineRpcClient,
};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
use jsonrpsee::types::ErrorCode;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
struct NoopDerivationClient;

#[async_trait]
impl EngineDerivationClient for NoopDerivationClient {
    async fn notify_sync_completed(
        &self,
        _: L2BlockInfo,
    ) -> Result<(), base_consensus_node::DerivationClientError> {
        Ok(())
    }

    async fn send_new_engine_safe_head(
        &self,
        _: L2BlockInfo,
    ) -> Result<(), base_consensus_node::DerivationClientError> {
        Ok(())
    }

    async fn send_signal(
        &self,
        _: base_consensus_derive::Signal,
    ) -> Result<(), base_consensus_node::DerivationClientError> {
        Ok(())
    }
}

const fn syncing_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Syncing,
            latest_valid_hash: None,
        },
        payload_id: None,
    }
}

fn mismatched_block(number: u64) -> RpcBlock<BaseTransaction> {
    let mut block = RpcBlock::<BaseTransaction>::default();
    block.header.hash = B256::from([0xabu8; 32]);
    block.header.inner.number = number;
    block.header.inner.timestamp = number * 2;
    block
}

#[derive(Debug)]
struct CountingEngineReceiver {
    builds_processed: Arc<AtomicU64>,
}

impl EngineRequestReceiver for CountingEngineReceiver {
    fn start(
        self,
        mut request_channel: mpsc::Receiver<EngineProcessingRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        let builds_processed = self.builds_processed;
        tokio::spawn(async move {
            loop {
                let Some(request) = request_channel.recv().await else {
                    return Err(EngineError::ChannelClosed);
                };

                if let EngineProcessingRequest::Build(build_request) = request {
                    builds_processed.fetch_add(1, Ordering::SeqCst);
                    let payload_id = PayloadId::new([0x01; 8]);
                    let _ = build_request.result_tx.send(payload_id).await;
                }
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn follow_restart_delegated_forkchoice_does_not_finalize_past_actual_safe_head() {
    let unsafe_head = test_block_info(100);
    let delegated_safe_number = 80;

    let initial_state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(L2BlockInfo::default())
        .with_finalized_head(L2BlockInfo::default())
        .with_el_sync_finished(false)
        .build();

    let client = Arc::new(
        test_engine_client_builder()
            .with_block_info_by_tag(alloy_eips::BlockNumberOrTag::Latest, unsafe_head)
            .with_l2_block_by_label(
                alloy_eips::BlockNumberOrTag::Number(delegated_safe_number),
                mismatched_block(delegated_safe_number),
            )
            .with_fork_choice_updated_v3_response(syncing_fcu())
            .build(),
    );

    let delegated_safe = L2BlockInfo {
        block_info: BlockInfo {
            number: delegated_safe_number,
            hash: B256::from([0xcdu8; 32]),
            ..Default::default()
        },
        ..Default::default()
    };

    let (state_tx, state_rx) = watch::channel(initial_state);
    let (queue_tx, _) = watch::channel(0usize);
    let engine = Engine::new(initial_state, state_tx, queue_tx);

    let processor = EngineProcessor::new(
        Arc::clone(&client),
        Arc::new(RollupConfig::default()),
        NoopDerivationClient,
        engine,
        EngineProcessorOptions {
            node_mode: NodeMode::Validator,
            unsafe_head_tx: None,
            conductor: None,
            sequencer_stopped: false,
        },
    );

    let (req_tx, req_rx) = mpsc::channel(8);
    let handle = processor.start(req_rx);

    state_rx
        .clone()
        .wait_for(|state| {
            state.sync_state.unsafe_head().block_info.number == unsafe_head.block_info.number
        })
        .await
        .expect("bootstrap did not seed unsafe head");

    req_tx
        .send(EngineProcessingRequest::ProcessDelegatedForkchoiceUpdate(Box::new(
            DelegatedForkchoiceUpdate {
                safe_l2: delegated_safe,
                finalized_l2_number: Some(delegated_safe_number),
            },
        )))
        .await
        .expect("failed to send delegated forkchoice update");

    drop(req_tx);
    let result = handle.await.expect("processor task panicked");
    assert!(
        matches!(result, Err(EngineError::ChannelClosed)),
        "expected ChannelClosed after request channel shutdown, got {result:?}"
    );

    let state = *state_rx.borrow();
    assert_eq!(
        state.sync_state.safe_head(),
        L2BlockInfo::default(),
        "safe head should remain unchanged when the delegated safe FCU returns Syncing",
    );
    assert_eq!(
        state.sync_state.finalized_head(),
        L2BlockInfo::default(),
        "finalized head must not advance past the actual engine safe head",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn full_public_rpc_queue_does_not_block_engine_processing_requests() {
    let cancellation_token = CancellationToken::new();
    let (engine_actor_request_tx, engine_actor_request_rx) = mpsc::channel(8);
    let (engine_rpc_request_tx, _engine_rpc_request_rx) = mpsc::channel(1);
    let builds_processed = Arc::new(AtomicU64::new(0));

    let engine_actor = EngineActor::new(
        cancellation_token.clone(),
        engine_actor_request_rx,
        CountingEngineReceiver { builds_processed: Arc::clone(&builds_processed) },
    );
    let engine_handle = tokio::spawn(async move { engine_actor.start(()).await });

    let client = QueuedEngineRpcClient::new(engine_rpc_request_tx);
    let (queued_response_tx, _queued_response_rx) = oneshot::channel();
    client
        .try_enqueue_engine_query(EngineQueries::TaskQueueLength(queued_response_tx))
        .expect("failed to fill public engine rpc queue");

    let (rejected_response_tx, _rejected_response_rx) = oneshot::channel();
    let error = client
        .try_enqueue_engine_query(EngineQueries::TaskQueueLength(rejected_response_tx))
        .expect_err("full public queue should reject public RPC requests");
    assert_eq!(error.code(), ErrorCode::ServerIsBusy.code());

    let (payload_id_tx, mut payload_id_rx) = mpsc::channel(1);
    let attributes = AttributesWithParent::new(
        BasePayloadAttributes::default(),
        L2BlockInfo::default(),
        None,
        true,
    );
    engine_actor_request_tx
        .send(EngineActorRequest::BuildRequest(Box::new(BuildRequest {
            attributes,
            result_tx: payload_id_tx,
        })))
        .await
        .expect("failed to enqueue build request");

    let payload_id = tokio::time::timeout(Duration::from_secs(2), payload_id_rx.recv())
        .await
        .expect("build request was blocked behind rpc backpressure")
        .expect("build response channel closed");

    assert_eq!(payload_id, PayloadId::new([0x01; 8]));
    assert_eq!(builds_processed.load(Ordering::SeqCst), 1);

    cancellation_token.cancel();
    drop(engine_actor_request_tx);
    let actor_result = tokio::time::timeout(Duration::from_secs(2), engine_handle).await;
    assert!(
        matches!(actor_result, Ok(Ok(Ok(()))) | Ok(Ok(Err(EngineError::ChannelClosed)))),
        "unexpected engine actor shutdown result: {actor_result:?}",
    );
}

#[tokio::test]
async fn queued_engine_rpc_client_rejects_when_public_rpc_queue_is_full() {
    let (engine_rpc_request_tx, _engine_rpc_request_rx) = mpsc::channel(1);
    let client = QueuedEngineRpcClient::new(engine_rpc_request_tx);

    let (queued_response_tx, _queued_response_rx) = oneshot::channel();
    client
        .try_enqueue_engine_query(EngineQueries::TaskQueueLength(queued_response_tx))
        .expect("failed to fill public engine rpc queue");

    let (rejected_response_tx, _rejected_response_rx) = oneshot::channel();
    let error = client
        .try_enqueue_engine_query(EngineQueries::TaskQueueLength(rejected_response_tx))
        .expect_err("full queue should reject public RPC requests");

    assert_eq!(error.code(), ErrorCode::ServerIsBusy.code());
}
