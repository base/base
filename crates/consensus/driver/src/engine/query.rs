//! Engine query interface for external communication.
//!
//! Provides a channel-based API for querying engine state and configuration
//! from external actors. Uses oneshot channels for responses to maintain
//! clean async communication patterns.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use base_common_chain_config::RollupConfig;
use base_consensus_batch::{L2BlockInfo, OutputRoot};
use tokio::sync::oneshot::Sender;

use crate::engine::{EngineClient, EngineClientError, EngineState};

/// Channel sender for submitting [`EngineQueries`] to the engine.
pub type EngineQuerySender = tokio::sync::mpsc::Sender<EngineQueries>;

/// Query types supported by the engine for external communication.
///
/// Each variant includes a oneshot sender for the response, enabling
/// async request-response patterns. The engine processes these queries
/// and sends responses back through the provided channels.
#[derive(Debug)]
pub enum EngineQueries {
    /// Request the current rollup configuration.
    Config(Sender<RollupConfig>),
    /// Request the current [`EngineState`] snapshot.
    State(Sender<EngineState>),
    /// Request the L2 output root for a specific block.
    ///
    /// Returns a tuple of block info, output root, and engine state at the requested block.
    OutputAtBlock {
        /// The block number or tag to retrieve the output for.
        block: BlockNumberOrTag,
        /// Response channel for (`block_info`, `output_root`, `engine_state`).
        sender: Sender<(L2BlockInfo, OutputRoot, EngineState)>,
    },
    /// Subscribe to engine state updates via a watch channel receiver.
    StateReceiver(Sender<tokio::sync::watch::Receiver<EngineState>>),
    /// Development API: Subscribe to task queue length updates.
    QueueLengthReceiver(Sender<tokio::sync::watch::Receiver<usize>>),
    /// Development API: Get the current number of pending tasks in the queue.
    TaskQueueLength(Sender<usize>),
}

/// An error that can occur when querying the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineQueriesError {
    /// The output channel was closed unexpectedly. Impossible to send query response.
    #[error("Output channel closed unexpectedly. Impossible to send query response")]
    OutputChannelClosed,
    /// Failed to retrieve the L2 block by label.
    #[error("Failed to retrieve L2 block by label: {0}")]
    BlockRetrievalFailed(#[from] EngineClientError),
    /// No block withdrawals root while Isthmus is active.
    #[error("No block withdrawals root while Isthmus is active")]
    NoWithdrawalsRoot,
    /// No L2 block found for block number or tag.
    #[error("No L2 block found for block number or tag: {0}")]
    NoL2BlockFound(BlockNumberOrTag),
}

impl EngineQueries {
    /// Handles the engine query request.
    pub async fn handle<EngineClient_: EngineClient>(
        self,
        state_recv: &tokio::sync::watch::Receiver<EngineState>,
        queue_length_recv: &tokio::sync::watch::Receiver<usize>,
        client: &Arc<EngineClient_>,
        rollup_config: &Arc<RollupConfig>,
    ) -> Result<(), EngineQueriesError> {
        let state = *state_recv.borrow();

        match self {
            Self::Config(sender) => sender
                .send(rollup_config.with_runtime_upgrade_overrides())
                .map_err(|_| EngineQueriesError::OutputChannelClosed),
            Self::State(sender) => {
                trace!(target: "engine", "Preparing engine state RPC response");
                sender.send(state).map_err(|_| EngineQueriesError::OutputChannelClosed)
            }
            Self::OutputAtBlock { block, sender } => {
                trace!(target: "engine", block = ?block, "Querying engine output at block");
                let output_block = client.l2_block_by_label(block).await?;
                let output_block = output_block.ok_or(EngineQueriesError::NoL2BlockFound(block))?;
                let block_hash = output_block.hash();
                let output_block = output_block.into_block();
                let output_block_info =
                    base_consensus_batch::L2BlockInfoDecoder::from_block_and_genesis(
                        &output_block,
                        &rollup_config.genesis,
                    )
                    .map_err(|_| EngineQueriesError::NoL2BlockFound(block))?;

                let state_root = output_block.header.state_root;

                let message_passer_storage_root = {
                    output_block
                        .header
                        .withdrawals_root
                        .ok_or(EngineQueriesError::NoWithdrawalsRoot)?
                };

                let output_response_v0 =
                    OutputRoot::from_parts(state_root, message_passer_storage_root, block_hash);

                trace!(target: "engine", block = ?block, "Sending engine output response");
                sender
                    .send((output_block_info, output_response_v0, state))
                    .map_err(|_| EngineQueriesError::OutputChannelClosed)
            }
            Self::StateReceiver(subscription) => subscription
                .send(state_recv.clone())
                .map_err(|_| EngineQueriesError::OutputChannelClosed),
            Self::QueueLengthReceiver(subscription) => subscription
                .send(queue_length_recv.clone())
                .map_err(|_| EngineQueriesError::OutputChannelClosed),
            Self::TaskQueueLength(sender) => {
                let queue_length = *queue_length_recv.borrow();
                if sender.send(queue_length).is_err() {
                    warn!(target: "engine", "Failed to send task queue length response");
                }
                Ok(())
            }
        }
    }
}
