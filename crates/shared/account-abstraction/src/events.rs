//! Events emitted by the user operation mempool.

use serde::{Deserialize, Serialize};

use crate::types::WrappedUserOperation;

/// Events that can be emitted by the mempool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum MempoolEvent {
    /// A user operation was added to the mempool.
    UserOpAdded {
        /// The user operation that was added.
        user_op: WrappedUserOperation,
    },
    /// A user operation was included in a block.
    UserOpIncluded {
        /// The user operation that was included.
        user_op: WrappedUserOperation,
    },
    /// A user operation was dropped from the mempool.
    UserOpDropped {
        /// The user operation that was dropped.
        user_op: WrappedUserOperation,
        /// The reason the operation was dropped.
        reason: String,
    },
}
