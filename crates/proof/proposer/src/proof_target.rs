//! Shared helpers for proposal target blocks.

use alloy_primitives::B256;
use base_proof_rpc::RollupProvider;
use tracing::{error, warn};

/// Shared proposal target helpers.
#[derive(Debug)]
pub struct ProofTarget;

impl ProofTarget {
    /// Computes the next proposal target from a current parent block and interval.
    pub fn next_block(current_block: u64, block_interval: u64) -> Option<u64> {
        if block_interval == 0 {
            error!("Block interval must be non-zero");
            return None;
        }

        current_block.checked_add(block_interval).or_else(|| {
            error!(current_block, block_interval, "Overflow computing next target block");
            None
        })
    }

    /// Fetches the canonical output root for a proposal target.
    pub async fn canonical_output_root<R>(rollup_client: &R, target_block: u64) -> Option<B256>
    where
        R: RollupProvider + ?Sized,
    {
        match rollup_client.output_at_block(target_block).await {
            Ok(output) => Some(output.output_root),
            Err(e) => {
                warn!(
                    target_block,
                    error = %e,
                    "Failed to fetch canonical output root"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_block_handles_interval_cases() {
        for (current_block, block_interval, expected) in
            [(100, 0, None), (100, 100, Some(200)), (150, 100, Some(250)), (u64::MAX, 1, None)]
        {
            assert_eq!(
                ProofTarget::next_block(current_block, block_interval),
                expected,
                "next_block({current_block}, {block_interval})"
            );
        }
    }
}
