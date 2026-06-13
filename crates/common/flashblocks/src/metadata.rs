//! Contains the [`Metadata`] type used in Flashblocks.

use base_access_lists::FlashblockAccessList;
use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// Block number this flashblock belongs to.
    pub block_number: u64,
    /// The flashblock access list — state diffs + read addresses from builder execution.
    /// When present, consumers can use this to warm up the EVM database before re-executing,
    /// eliminating the execution divergence described in #3274.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_list: Option<FlashblockAccessList>,
}
