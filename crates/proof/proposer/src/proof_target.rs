//! The next proposal target of a game chain.

use alloy_primitives::B256;
use base_proof_rpc::RollupProvider;
use tracing::warn;

use crate::proposal_intervals::Intervals;

/// The proposal that follows a game chain, as returned by
/// [`crate::IntervalResolver::next_target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofTarget {
    /// L2 block number the proposal claims.
    pub block: u64,
    /// Intervals of the game that ends at `block`.
    pub intervals: Intervals,
}

impl ProofTarget {
    /// Fetches the canonical output root at the target block, logging and returning `None`
    /// on failure.
    pub async fn canonical_output_root<R>(&self, rollup_client: &R) -> Option<B256>
    where
        R: RollupProvider + ?Sized,
    {
        match rollup_client.output_at_block(self.block).await {
            Ok(output) => Some(output.output_root),
            Err(e) => {
                warn!(
                    target_block = self.block,
                    error = %e,
                    "Failed to fetch canonical output root"
                );
                None
            }
        }
    }
}
