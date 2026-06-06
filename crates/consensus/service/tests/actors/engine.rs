//! Integration tests for the engine processing path.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_rpc_types_engine::PayloadId;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_engine::EngineQueries;
use base_consensus_node::{
    BuildRequest, EngineActor, EngineActorRequest, EngineError, EngineRequestReceiver, NodeActor,
    QueuedEngineRpcClient,
};
use base_protocol::{AttributesWithParent, L2BlockInfo};
use jsonrpsee::types::ErrorCode;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct CountingEngineReceiver {
    builds_processed: Arc<AtomicU64>,
}

impl EngineRequestReceiver for CountingEngineReceiver {
    fn start(
        self,
        mut request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        let builds_processed = self.builds_processed;
        tokio::spawn(async move {
            loop {
                let Some(request) = request_channel.recv().await else {
                    return Err(EngineError::ChannelClosed);
                };

                if let EngineActorRequest::BuildRequest(build_request) = request {
                    builds_processed.fetch_add(1, Ordering::SeqCst);
                    let payload_id = PayloadId::new([0x01; 8]);
                    let _ = build_request.result_tx.send(Ok(payload_id)).await;
                }
            }
        })
    }
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
        .expect("build response channel closed")
        .expect("build request failed");

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
