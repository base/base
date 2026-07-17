use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_consensus_engine::{EngineTaskError, EngineTaskErrorSeverity};
use base_protocol::FromBlockError;
use thiserror::Error;

use crate::follow::source::RemoteL2ClientError;

/// Error returned by follow-mode runtime, client, engine, and RPC operations.
#[derive(Debug, Error)]
pub enum FollowError {
    /// The local L2 node did not return a block for the requested tag.
    #[error("local L2 block unavailable at {0:?}")]
    LocalBlockUnavailable(BlockNumberOrTag),

    /// Fetching a block from the local L2 node failed.
    #[error("failed to fetch local L2 block at {tag:?}: {source}")]
    LocalBlockFetch {
        /// Requested local block tag.
        tag: BlockNumberOrTag,
        /// Underlying transport error.
        source: alloy_transport::TransportError,
    },

    /// Converting a local L2 block into block info failed.
    #[error("failed to build local L2 block info: {0}")]
    LocalBlockInfo(#[from] FromBlockError),

    /// Fetching a block from the local L1 node failed.
    #[error("failed to fetch local L1 block {number}: {source}")]
    LocalL1BlockFetch {
        /// Requested L1 block number.
        number: u64,
        /// Underlying transport error.
        source: alloy_transport::TransportError,
    },

    /// The local L1 node did not return the source label's claimed L1 origin.
    #[error("local L1 block unavailable at block {0}")]
    LocalL1BlockUnavailable(u64),

    /// A source L2 label claims an L1 origin that is not canonical locally.
    #[error(
        "source L2 block {l2_number} claims L1 origin {remote} at block {l1_number}, but local L1 has {local}"
    )]
    SourceL1OriginMismatch {
        /// Source L2 label block number.
        l2_number: u64,
        /// Claimed L1 origin block number.
        l1_number: u64,
        /// Hash from the local canonical L1 provider.
        local: B256,
        /// L1 origin hash encoded in the source L2 block.
        remote: B256,
    },

    /// Fetching the local proofs sync status failed.
    #[error("failed to fetch proofs sync status: {0}")]
    ProofsStatus(alloy_transport::TransportError),

    /// Fetching data from the remote L2 source failed.
    #[error(transparent)]
    Remote(#[from] RemoteL2ClientError),

    /// The source and local L2 nodes returned different hashes for the same block number.
    #[error("source block hash {remote} does not match local block hash {local} at block {number}")]
    SourceBlockHashMismatch {
        /// Block number compared across the source and local nodes.
        number: u64,
        /// Hash returned by the local L2 node.
        local: B256,
        /// Hash returned by the source L2 node.
        remote: B256,
    },

    /// Two source reads for the same block number described different source branches.
    #[error("source branch mismatch at block {number}: expected {expected}, got {actual}")]
    SourceBranchMismatch {
        /// Block number compared across source reads.
        number: u64,
        /// Hash from the source branch captured earlier.
        expected: B256,
        /// Hash returned by the later source read.
        actual: B256,
    },

    /// A source block's parent lookup did not return its direct parent.
    #[error(
        "source chain is discontinuous between child block {child_number} and parent block {parent_number}"
    )]
    SourceChainDiscontinuity {
        /// Child block number.
        child_number: u64,
        /// Number reported by the block fetched using the child's parent hash.
        parent_number: u64,
    },

    /// Recovery exceeded its shared lookup or time budget.
    #[error("follow recovery budget exceeded during {phase} after {lookups} lookups")]
    RecoveryBudgetExceeded {
        /// Recovery phase that exhausted the budget.
        phase: &'static str,
        /// Number of lookups attempted before the budget was exhausted.
        lookups: u64,
    },

    /// A recovery reorg was refused because it would rewind below the finalized head.
    #[error("refusing to reorg to block {number} below the finalized head {finalized}")]
    ReorgBelowFinalized {
        /// Block number we were asked to reorg to.
        number: u64,
        /// Current local finalized head number.
        finalized: u64,
    },

    /// The engine did not confirm the forkchoice reset to the common ancestor. This usually means
    /// the EL returned `Syncing`, so recovery cannot safely replay payloads yet.
    #[error("engine did not confirm reset to ancestor block {number} ({hash})")]
    ResetToAncestorUnconfirmed {
        /// Ancestor block number.
        number: u64,
        /// Ancestor block hash.
        hash: B256,
    },

    /// The local engine rejected a follow-mode task.
    #[error("engine task failed with {severity} severity: {error}")]
    EngineTask {
        /// Engine task error severity.
        severity: EngineTaskErrorSeverity,
        /// Engine task error message.
        error: String,
    },

    /// Starting or restarting the follow RPC server failed.
    #[error("follow RPC server failed: {0}")]
    RpcServer(String),

    /// Building the follow RPC module failed.
    #[error("follow RPC module failed: {0}")]
    RpcModule(String),

    /// Stopping the follow RPC server failed.
    #[error("follow RPC server stop failed: {0}")]
    RpcStop(String),

    /// The follow RPC server exceeded its restart limit.
    #[error("follow RPC server stopped too many times")]
    RpcRestartLimit,

    /// The insert loop lost its payload producer.
    #[error("blocks-to-insert channel closed")]
    BlocksToInsertChannelClosed,

    /// The insert loop received a payload for the wrong block number.
    #[error("prefetcher returned block {actual}, expected {expected}")]
    OutOfOrderPayload {
        /// Payload block number received from the prefetcher.
        actual: u64,
        /// Block number the insert loop expected next.
        expected: u64,
    },

    /// The execution engine did not advance to the source payload that was inserted.
    #[error(
        "engine did not advance to source payload {expected_hash} at block {expected_number}; \
         current head is {actual_hash} at block {actual_number}"
    )]
    PayloadNotApplied {
        /// Source payload block number.
        expected_number: u64,
        /// Source payload block hash.
        expected_hash: B256,
        /// Engine head block number after insertion.
        actual_number: u64,
        /// Engine head block hash after insertion.
        actual_hash: B256,
    },

    /// Joining a follow-mode task failed.
    #[error("follow task join failed: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

impl FollowError {
    /// Builds a follow error from an engine task error while preserving severity.
    pub fn engine_task(error: impl EngineTaskError + ToString) -> Self {
        let severity = error.severity();
        Self::EngineTask { severity, error: error.to_string() }
    }
}
