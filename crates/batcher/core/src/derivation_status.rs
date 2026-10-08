//! Derivation progress consumed by the batcher driver.

use base_protocol::BlockInfo;

/// The derivation progress of the rollup node the batcher follows.
///
/// The two fields are read from the node together but not atomically, so they can be one
/// derivation step apart.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DerivationStatus {
    /// The safe L2 head derivation has reached.
    pub safe_l2: BlockInfo,
    /// The L1 block derivation is processing. Every earlier L1 block, and the batcher data it
    /// carried, has been read.
    pub current_l1: BlockInfo,
}

impl DerivationStatus {
    /// Whether the status is one a starting node reports: without a safe L2 head until its
    /// engine is bootstrapped, or without an L1 block until its derivation pipeline has an
    /// origin. Such a status says nothing about derivation progress.
    pub fn is_from_a_starting_node(&self) -> bool {
        self.safe_l2 == BlockInfo::default() || self.current_l1 == BlockInfo::default()
    }
}
