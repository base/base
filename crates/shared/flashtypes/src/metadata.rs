//! Contains the [`Metadata`] type used in Flashblocks.

use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// Block number this flashblock belongs to.
    pub block_number: u64,
}
