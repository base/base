use std::fmt::Debug;

use alloy_eips::BlockNumberOrTag;
use base_common_chain_config::RollupConfig;
use base_consensus_batch::{L2BlockInfo, OutputRoot};
use derive_more::Constructor;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};
use tokio::sync::{
    mpsc::{self, error::TrySendError},
    oneshot, watch,
};

use crate::{EngineQueries, EngineRpcRequest, EngineState};

/// Sends engine RPC requests through the bounded driver queue.
#[derive(Clone, Constructor, Debug)]
pub struct EngineRpcClient {
    /// A channel to use to send engine RPC requests.
    pub engine_rpc_request_tx: mpsc::Sender<EngineRpcRequest>,
}

impl EngineRpcClient {
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

    /// Request the current [`RollupConfig`].
    pub async fn get_config(&self) -> RpcResult<RollupConfig> {
        let (config_tx, config_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::Config(config_tx))?;

        config_rx.await.map_err(|_| {
            error!(target: "block_engine", "Failed to receive config from engine rpc");
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    /// Request the current [`EngineState`] snapshot.
    pub async fn get_state(&self) -> RpcResult<EngineState> {
        let (state_tx, state_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::State(state_tx))?;

        state_rx.await.map_err(|_| {
            error!(target: "block_engine", "Failed to receive state from engine rpc");
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    /// Request the L2 output root for a specific [`BlockNumberOrTag`].
    ///
    /// Returns a tuple of [`L2BlockInfo`], [`OutputRoot`], and [`EngineState`] at the requested
    /// block.
    pub async fn output_at_block(
        &self,
        block: BlockNumberOrTag,
    ) -> RpcResult<(L2BlockInfo, OutputRoot, EngineState)> {
        let (output_tx, output_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::OutputAtBlock { block, sender: output_tx })?;

        output_rx.await.map_err(|_| {
            error!(target: "block_engine", "Failed to receive output at block from engine rpc");
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    /// Development API: Get the current number of pending tasks in the queue.
    pub async fn dev_get_task_queue_length(&self) -> RpcResult<usize> {
        let (length_tx, length_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::TaskQueueLength(length_tx))?;

        length_rx.await.map_err(|_| {
            error!(target: "block_engine", "Failed to receive task queue length from engine rpc");
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    /// Development API: Subscribes to engine queue length updates managed by the returned
    /// [`watch::Receiver`].
    pub async fn dev_subscribe_to_engine_queue_length(&self) -> RpcResult<watch::Receiver<usize>> {
        let (sub_tx, sub_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::QueueLengthReceiver(sub_tx))?;

        sub_rx.await.map_err(|_| {
            error!(target: "block_engine", "Failed to receive queue length receiver from engine rpc");
            ErrorObject::from(ErrorCode::InternalError)
        })
    }

    /// Development API: Subscribes to engine state updates managed by the returned
    /// [`watch::Receiver`].
    pub async fn dev_subscribe_to_engine_state(&self) -> RpcResult<watch::Receiver<EngineState>> {
        let (sub_tx, sub_rx) = oneshot::channel();

        self.try_enqueue_engine_query(EngineQueries::StateReceiver(sub_tx))?;

        sub_rx.await.map_err(|_| {
            error!(target: "block_engine", "Failed to receive state receiver from engine rpc");
            ErrorObject::from(ErrorCode::InternalError)
        })
    }
}
