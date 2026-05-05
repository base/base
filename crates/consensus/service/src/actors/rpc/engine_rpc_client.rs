use std::fmt::Debug;

use alloy_eips::BlockNumberOrTag;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_consensus_engine::{EngineQueries, EngineState};
use base_consensus_rpc::EngineRpcClient;
use base_protocol::{L2BlockInfo, OutputRoot};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};
use tokio::sync::{
    mpsc::{self, error::TrySendError},
    oneshot, watch,
};

use super::QueuedRpcClientOptions;
use crate::EngineRpcRequest;

/// Queue-based implementation of the [`EngineRpcClient`] trait. This handles all channel-based
/// operations, providing a nice facade for callers.
#[derive(Clone, Debug)]
pub struct QueuedEngineRpcClient {
    /// A channel to use to send engine RPC requests.
    pub engine_rpc_request_tx: mpsc::Sender<EngineRpcRequest>,
    /// Runtime options for request/response handling.
    pub options: QueuedRpcClientOptions,
}

impl QueuedEngineRpcClient {
    /// Creates a new queued engine RPC client with historical no-timeout behavior.
    pub fn new(engine_rpc_request_tx: mpsc::Sender<EngineRpcRequest>) -> Self {
        Self::new_with_options(engine_rpc_request_tx, QueuedRpcClientOptions::default())
    }

    /// Creates a new queued engine RPC client with explicit runtime options.
    pub const fn new_with_options(
        engine_rpc_request_tx: mpsc::Sender<EngineRpcRequest>,
        options: QueuedRpcClientOptions,
    ) -> Self {
        Self { engine_rpc_request_tx, options }
    }

    /// Attempts to enqueue an engine query without waiting for channel capacity.
    ///
    /// Public RPC requests fail fast under load so they cannot block consensus-critical work.
    pub fn try_enqueue_engine_query(&self, query: EngineQueries) -> RpcResult<()> {
        self.engine_rpc_request_tx.try_send(EngineRpcRequest::EngineQuery(Box::new(query))).map_err(
            |error| match error {
                TrySendError::Full(_) => {
                    warn!(target: "block_engine", "Engine RPC request queue full");
                    ErrorObject::from(ErrorCode::ServerIsBusy)
                }
                TrySendError::Closed(_) => {
                    error!(target: "block_engine", "Failed to enqueue engine RPC request");
                    ErrorObject::from(ErrorCode::InternalError)
                }
            },
        )
    }

    /// Awaits an engine actor response using the configured timeout policy.
    pub async fn receive_engine_response<T>(
        &self,
        response_rx: oneshot::Receiver<T>,
        response_name: &'static str,
    ) -> RpcResult<T> {
        let receive = async {
            response_rx.await.map_err(|_| {
                error!(
                    target: "block_engine",
                    response_name,
                    "Failed to receive response from engine rpc"
                );
                ErrorObject::from(ErrorCode::InternalError)
            })
        };

        if let Some(timeout) = self.options.request_timeout {
            tokio::time::timeout(timeout, receive).await.map_err(|_| {
                error!(
                    target: "block_engine",
                    response_name,
                    timeout_ms = timeout.as_millis(),
                    "Timed out waiting for engine rpc response"
                );
                ErrorObject::owned(
                    ErrorCode::InternalError.code(),
                    "engine rpc response timed out",
                    None::<()>,
                )
            })?
        } else {
            receive.await
        }
    }
}

#[async_trait]
impl EngineRpcClient for QueuedEngineRpcClient {
    async fn get_config(&self) -> RpcResult<RollupConfig> {
        let (config_tx, config_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::Config(config_tx))?;

        self.receive_engine_response(config_rx, "config").await
    }

    async fn get_state(&self) -> RpcResult<EngineState> {
        let (state_tx, state_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::State(state_tx))?;

        self.receive_engine_response(state_rx, "state").await
    }

    async fn output_at_block(
        &self,
        block: BlockNumberOrTag,
    ) -> RpcResult<(L2BlockInfo, OutputRoot, EngineState)> {
        let (output_tx, output_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::OutputAtBlock { block, sender: output_tx })?;

        self.receive_engine_response(output_rx, "output_at_block").await
    }

    async fn dev_get_task_queue_length(&self) -> RpcResult<usize> {
        let (length_tx, length_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::TaskQueueLength(length_tx))?;

        self.receive_engine_response(length_rx, "task_queue_length").await
    }

    async fn dev_subscribe_to_engine_queue_length(&self) -> RpcResult<watch::Receiver<usize>> {
        let (sub_tx, sub_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::QueueLengthReceiver(sub_tx))?;

        self.receive_engine_response(sub_rx, "queue_length_receiver").await
    }

    async fn dev_subscribe_to_engine_state(&self) -> RpcResult<watch::Receiver<EngineState>> {
        let (sub_tx, sub_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::StateReceiver(sub_tx))?;

        self.receive_engine_response(sub_rx, "state_receiver").await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base_consensus_rpc::EngineRpcClient as _;

    use super::*;

    #[tokio::test]
    async fn get_state_times_out_when_response_is_not_sent() {
        let (engine_rpc_request_tx, mut engine_rpc_request_rx) = mpsc::channel(1);
        let client = QueuedEngineRpcClient::new_with_options(
            engine_rpc_request_tx,
            QueuedRpcClientOptions { request_timeout: Some(Duration::ZERO) },
        );

        let result = client.get_state().await;

        assert!(result.is_err(), "client should return timeout error");
        let request = engine_rpc_request_rx.recv().await.expect("request should be queued");
        assert!(
            matches!(request, EngineRpcRequest::EngineQuery(_)),
            "expected queued engine query"
        );
    }
}
