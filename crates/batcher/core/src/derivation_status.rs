//! Derivation progress consumed by the batcher driver.

use base_protocol::BlockInfo;

/// The derivation progress of the rollup node the batcher follows.
///
/// The safe head includes every block derived from the L1 blocks before `current_l1`: a node
/// moves its derivation past an L1 block only once its engine has made those blocks safe, and
/// reads `current_l1` before the safe head.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DerivationStatus {
    /// The safe L2 head derivation has reached.
    pub safe_l2: BlockInfo,
    /// The L1 block derivation is processing. Every earlier L1 block, and the batcher data it
    /// carried, has been read.
    pub current_l1: BlockInfo,
}

impl DerivationStatus {
    /// Whether the status lacks a safe L2 head or an L1 block. A node reports them empty until
    /// its engine is bootstrapped and its derivation pipeline has an origin, as at startup or
    /// during execution-layer sync.
    pub fn lacks_safe_head_or_l1_block(&self) -> bool {
        self.safe_l2 == BlockInfo::default() || self.current_l1 == BlockInfo::default()
    }
}
