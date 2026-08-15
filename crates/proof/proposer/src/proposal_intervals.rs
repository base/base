//! Computes proposal checkpoint block intervals shared by recovery and submission.

use base_proof_contracts::game_lookup_blocks;

use crate::error::ProposerError;

/// Shared proposal interval calculations.
#[derive(Debug)]
pub struct ProposalIntervals;

impl ProposalIntervals {
    /// Returns intermediate block numbers between `starting_block_number` and
    /// the next proposal target, stepping by `intermediate_block_interval`.
    pub fn intermediate_block_numbers(
        block_interval: u64,
        intermediate_block_interval: u64,
        starting_block_number: u64,
    ) -> Result<Vec<u64>, ProposerError> {
        game_lookup_blocks(starting_block_number, block_interval, intermediate_block_interval)
            .map_err(|e| ProposerError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intermediate_block_numbers_rejects_zero_block_interval() {
        let result = ProposalIntervals::intermediate_block_numbers(0, 1, 0);

        assert!(
            matches!(result, Err(ProposerError::Config(message)) if message == "block_interval must not be zero")
        );
    }
}
