//! Derivation progress consumed by the batcher driver.

use base_protocol::BlockInfo;

/// A coherent snapshot of the derivation progress relevant to the batcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DerivationStatus {
    /// The trusted safe L2 head reported by the derivation provider.
    pub safe_l2: BlockInfo,
    /// The L1 block currently being processed by derivation, when available.
    pub current_l1: Option<BlockInfo>,
}

impl DerivationStatus {
    /// Creates a status with both the safe L2 head and derivation cursor.
    pub const fn new(safe_l2: BlockInfo, current_l1: BlockInfo) -> Self {
        Self { safe_l2, current_l1: Some(current_l1) }
    }

    /// Creates a status for a provider that only exposes its safe L2 head.
    pub const fn from_safe_l2(safe_l2: BlockInfo) -> Self {
        Self { safe_l2, current_l1: None }
    }
}
