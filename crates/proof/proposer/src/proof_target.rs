//! Shared helpers for proposal target blocks.

use alloy_primitives::B256;
use base_proof_rpc::RollupProvider;
use tracing::{error, warn};

/// Shared proposal target helpers.
#[derive(Debug, Clone, Copy)]
pub struct ProofTarget;

impl ProofTarget {
    /// Computes the next proposal target from a current parent block and interval.
    pub fn next_block(current_block: u64, block_interval: u64) -> Option<u64> {
        if block_interval == 0 {
            error!("Block interval must be non-zero");
            return None;
        }

        current_block.checked_add(block_interval).map_or_else(
            || {
                error!(current_block, block_interval, "Overflow computing next target block");
                None
            },
            Some,
        )
    }

    /// Fetches the canonical output root for a proposal target.
    pub async fn canonical_output_root<R>(
        rollup_client: &R,
        target_block: u64,
        caller: &'static str,
    ) -> Option<B256>
    where
        R: RollupProvider,
    {
        match rollup_client.output_at_block(target_block).await {
            Ok(output) => Some(output.output_root),
            Err(e) => {
                warn!(
                    target_block,
                    caller = %caller,
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
    fn next_block_returns_none_for_zero_interval() {
        assert_eq!(ProofTarget::next_block(100, 0), None);
    }

    #[test]
    fn next_block_adds_interval_to_parent() {
        assert_eq!(ProofTarget::next_block(100, 100), Some(200));
        assert_eq!(ProofTarget::next_block(150, 100), Some(250));
    }

    #[test]
    fn next_block_returns_none_on_overflow() {
        assert_eq!(ProofTarget::next_block(u64::MAX, 1), None);
    }
}
