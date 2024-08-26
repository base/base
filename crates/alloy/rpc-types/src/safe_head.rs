//! Contains the response for a safe head request.

use alloy_rpc_types_eth::BlockId;
use serde::{Deserialize, Serialize};

/// The safe head response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeHeadResponse {
    /// The L1 block.
    pub l1_block: BlockId,
    /// The safe head.
    pub safe_head: BlockId,
}
