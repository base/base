//! Events emitted by the beacon consensus engine.

use alloc::{boxed::Box, string::String};
use core::{
    fmt::{Display, Formatter, Result},
    time::Duration,
};

use alloy_eips::BlockNumHash;
use base_common_rpc_types_engine::ForkchoiceState;
use reth_chain_state::{ExecutedBlock, ExecutionTimingStats};
use reth_primitives_traits::{SealedBlock, SealedHeader};

use crate::ForkchoiceStatus;

/// Type alias for backwards compat
#[deprecated(note = "Use ConsensusEngineEvent instead")]
pub type BeaconConsensusEngineEvent = ConsensusEngineEvent;

/// Events emitted by the consensus engine.
#[derive(Clone, Debug)]
pub enum ConsensusEngineEvent {
    /// The fork choice state was updated, and the current fork choice status
    ForkchoiceUpdated(ForkchoiceState, ForkchoiceStatus),
    /// A block was added to the fork chain.
    ForkBlockAdded(ExecutedBlock, Duration),
    /// A new block was received from the consensus engine
    BlockReceived(BlockNumHash),
    /// A block was added to the canonical chain, and the elapsed time validating the block
    CanonicalBlockAdded(ExecutedBlock, Duration),
    /// A canonical chain was committed, and the elapsed time committing the data
    CanonicalChainCommitted(Box<SealedHeader>, Duration),
    /// The consensus engine processed an invalid block.
    InvalidBlock {
        /// The invalid block.
        block: Box<SealedBlock>,
        /// The validation error that caused the block to be rejected.
        error: String,
    },
    /// A slow block was detected after persistence, with its timing statistics.
    SlowBlock(SlowBlockInfo),
}

impl ConsensusEngineEvent {
    /// Returns the canonical header if the event is a
    /// [`ConsensusEngineEvent::CanonicalChainCommitted`].
    pub const fn canonical_header(&self) -> Option<&SealedHeader> {
        match self {
            Self::CanonicalChainCommitted(header, _) => Some(header),
            _ => None,
        }
    }
}

impl Display for ConsensusEngineEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::ForkchoiceUpdated(state, status) => {
                write!(f, "ForkchoiceUpdated({state:?}, {status:?})")
            }
            Self::ForkBlockAdded(block, duration) => {
                write!(f, "ForkBlockAdded({:?}, {duration:?})", block.recovered_block.num_hash())
            }
            Self::CanonicalBlockAdded(block, duration) => {
                write!(
                    f,
                    "CanonicalBlockAdded({:?}, {duration:?})",
                    block.recovered_block.num_hash()
                )
            }
            Self::CanonicalChainCommitted(block, duration) => {
                write!(f, "CanonicalChainCommitted({:?}, {duration:?})", block.num_hash())
            }
            Self::InvalidBlock { block, error } => {
                write!(f, "InvalidBlock({:?}, {error})", block.num_hash())
            }
            Self::BlockReceived(num_hash) => {
                write!(f, "BlockReceived({num_hash:?})")
            }
            Self::SlowBlock(info) => {
                write!(
                    f,
                    "SlowBlock(block={}, total={:?})",
                    info.stats.block_number, info.total_duration
                )
            }
        }
    }
}

/// Information about a slow block detected after execution or persistence.
#[derive(Clone, Debug)]
pub struct SlowBlockInfo {
    /// The timing statistics for the slow block.
    pub stats: Box<ExecutionTimingStats>,
    /// The commit duration for the batch containing this block.
    /// `None` when emitted immediately after execution (before persistence).
    pub commit_duration: Option<Duration>,
    /// The total duration (execution + `state_root` + commit).
    /// Note: `state_read` is a subset of execution and is not added separately.
    pub total_duration: Duration,
}
