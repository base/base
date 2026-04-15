use std::{
    fmt::Debug,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use alloy_eips::BlockNumberOrTag;
use async_trait::async_trait;
use base_consensus_engine::{EngineQueries, EngineState};
use base_consensus_genesis::RollupConfig;
use base_consensus_rpc::EngineRpcClient;
use base_protocol::{L2BlockInfo, OutputRoot};
use derive_more::Constructor;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{EngineActorRequest, EngineRpcRequest};

static ENGINE_RPC_CLIENT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Queue-based implementation of the [`EngineRpcClient`] trait. This handles all channel-based
/// operations, providing a nice facade for callers. This also exposes only a subset of the
/// supported [`EngineActorRequest`] operations to limit the power of callers to RPC-type requests.
#[derive(Clone, Constructor, Debug)]
pub struct QueuedEngineRpcClient {
    /// A channel to use to send the `EngineActor` requests.
    pub engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
}

#[async_trait]
impl EngineRpcClient for QueuedEngineRpcClient {
    async fn get_config(&self) -> RpcResult<RollupConfig> {
        let request_id = ENGINE_RPC_CLIENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (config_tx, config_rx) = oneshot::channel();
        let send_started_at = Instant::now();

        info!(
            target: "block_engine",
            request_id,
            rpc_method = "optimism_rollupConfig",
            "Queueing engine RPC request"
        );

        self.engine_actor_request_tx
            .send(EngineActorRequest::RpcRequest(Box::new(EngineRpcRequest::EngineQuery {
                request_id,
                rpc_method: "optimism_rollupConfig",
                query: Box::new(EngineQueries::Config(config_tx)),
            })))
            .await
            .map_err(|_| {
                error!(
                    target: "block_engine",
                    request_id,
                    rpc_method = "optimism_rollupConfig",
                    elapsed_ms = send_started_at.elapsed().as_millis() as u64,
                    "Failed to enqueue engine RPC request"
                );
                ErrorObject::from(ErrorCode::InternalError)
            })?;

        info!(
            target: "block_engine",
            request_id,
            rpc_method = "optimism_rollupConfig",
            elapsed_ms = send_started_at.elapsed().as_millis() as u64,
            "Enqueued engine RPC request"
        );

        config_rx.await.map_err(|_| {
            error!(
                target: "block_engine",
                request_id,
                rpc_method = "optimism_rollupConfig",
                "Failed to receive engine RPC response"
            );
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    async fn get_state(&self) -> RpcResult<EngineState> {
        let request_id = ENGINE_RPC_CLIENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.get_state_with_context(request_id, "optimism_syncStatus").await
    }

    async fn get_state_with_context(
        &self,
        request_id: u64,
        rpc_method: &'static str,
    ) -> RpcResult<EngineState> {
        let (state_tx, state_rx) = oneshot::channel();
        let send_started_at = Instant::now();

        info!(
            target: "block_engine",
            request_id,
            rpc_method,
            "Queueing engine RPC request"
        );

        self.engine_actor_request_tx
            .send(EngineActorRequest::RpcRequest(Box::new(EngineRpcRequest::EngineQuery {
                request_id,
                rpc_method,
                query: Box::new(EngineQueries::State(state_tx)),
            })))
            .await
            .map_err(|_| {
                error!(
                    target: "block_engine",
                    request_id,
                    rpc_method,
                    elapsed_ms = send_started_at.elapsed().as_millis() as u64,
                    "Failed to enqueue engine RPC request"
                );
                ErrorObject::from(ErrorCode::InternalError)
            })?;

        info!(
            target: "block_engine",
            request_id,
            rpc_method,
            elapsed_ms = send_started_at.elapsed().as_millis() as u64,
            "Enqueued engine RPC request"
        );

        state_rx.await.map_err(|_| {
            error!(
                target: "block_engine",
                request_id,
                rpc_method,
                "Failed to receive engine RPC response"
            );
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    async fn output_at_block(
        &self,
        block: BlockNumberOrTag,
    ) -> RpcResult<(L2BlockInfo, OutputRoot, EngineState)> {
        let request_id = ENGINE_RPC_CLIENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.output_at_block_with_context(request_id, "optimism_outputAtBlock", block).await
    }

    async fn output_at_block_with_context(
        &self,
        request_id: u64,
        rpc_method: &'static str,
        block: BlockNumberOrTag,
    ) -> RpcResult<(L2BlockInfo, OutputRoot, EngineState)> {
        let (output_tx, output_rx) = oneshot::channel();
        let send_started_at = Instant::now();

        info!(
            target: "block_engine",
            request_id,
            rpc_method,
            block = ?block,
            "Queueing engine RPC request"
        );

        self.engine_actor_request_tx
            .send(EngineActorRequest::RpcRequest(Box::new(EngineRpcRequest::EngineQuery {
                request_id,
                rpc_method,
                query: Box::new(EngineQueries::OutputAtBlock { block, sender: output_tx }),
            })))
            .await
            .map_err(|_| {
                error!(
                    target: "block_engine",
                    request_id,
                    rpc_method,
                    block = ?block,
                    elapsed_ms = send_started_at.elapsed().as_millis() as u64,
                    "Failed to enqueue engine RPC request"
                );
                ErrorObject::from(ErrorCode::InternalError)
            })?;

        info!(
            target: "block_engine",
            request_id,
            rpc_method,
            block = ?block,
            elapsed_ms = send_started_at.elapsed().as_millis() as u64,
            "Enqueued engine RPC request"
        );

        output_rx.await.map_err(|_| {
            error!(
                target: "block_engine",
                request_id,
                rpc_method,
                block = ?block,
                "Failed to receive engine RPC response"
            );
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    async fn dev_get_task_queue_length(&self) -> RpcResult<usize> {
        let request_id = ENGINE_RPC_CLIENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (length_tx, length_rx) = oneshot::channel();

        self.engine_actor_request_tx
            .send(EngineActorRequest::RpcRequest(Box::new(EngineRpcRequest::EngineQuery {
                request_id,
                rpc_method: "debug_engineTaskQueueLength",
                query: Box::new(EngineQueries::TaskQueueLength(length_tx)),
            })))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        length_rx.await.map_err(|_| {
            error!(
                target: "block_engine",
                request_id,
                rpc_method = "debug_engineTaskQueueLength",
                "Failed to receive engine RPC response"
            );
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    async fn dev_subscribe_to_engine_queue_length(&self) -> RpcResult<watch::Receiver<usize>> {
        let request_id = ENGINE_RPC_CLIENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (sub_tx, sub_rx) = oneshot::channel();

        self.engine_actor_request_tx
            .send(EngineActorRequest::RpcRequest(Box::new(EngineRpcRequest::EngineQuery {
                request_id,
                rpc_method: "debug_subscribeToEngineQueueLength",
                query: Box::new(EngineQueries::QueueLengthReceiver(sub_tx)),
            })))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        sub_rx.await.map_err(|_| {
            error!(
                target: "block_engine",
                request_id,
                rpc_method = "debug_subscribeToEngineQueueLength",
                "Failed to receive engine RPC response"
            );
            ErrorObject::from(ErrorCode::InternalError)
        })
    }
    async fn dev_subscribe_to_engine_state(&self) -> RpcResult<watch::Receiver<EngineState>> {
        let request_id = ENGINE_RPC_CLIENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (sub_tx, sub_rx) = oneshot::channel();

        self.engine_actor_request_tx
            .send(EngineActorRequest::RpcRequest(Box::new(EngineRpcRequest::EngineQuery {
                request_id,
                rpc_method: "debug_subscribeToEngineState",
                query: Box::new(EngineQueries::StateReceiver(sub_tx)),
            })))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        sub_rx.await.map_err(|_| {
            error!(
                target: "block_engine",
                request_id,
                rpc_method = "debug_subscribeToEngineState",
                "Failed to receive engine RPC response"
            );
            ErrorObject::from(ErrorCode::InternalError)
        })
    }
}
