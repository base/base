//! Derivation progress consumed by the batcher driver.

use base_protocol::BlockInfo;

/// A coherent snapshot of the derivation progress relevant to the batcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DerivationStatus {
    /// The safe L2 head derivation has reached.
    pub safe_l2: BlockInfo,
    /// The L1 block derivation is processing. Every earlier L1 block, and the batcher data it
    /// carried, has been read.
    pub current_l1: BlockInfo,
}
