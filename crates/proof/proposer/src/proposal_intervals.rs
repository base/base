//! Computes proposal checkpoint block intervals shared by recovery and submission.

use crate::error::ProposerError;

/// Shared proposal interval calculations.
#[derive(Debug, Clone, Copy)]
pub struct ProposalIntervals;

impl ProposalIntervals {
    /// Returns intermediate block numbers between `starting_block_number` and
    /// the next proposal target, stepping by `intermediate_block_interval`.
    pub fn intermediate_block_numbers(
        block_interval: u64,
        intermediate_block_interval: u64,
        starting_block_number: u64,
    ) -> Result<Vec<u64>, ProposerError> {
        if intermediate_block_interval == 0 {
            return Err(ProposerError::Config(
                "intermediate_block_interval must not be zero".into(),
            ));
        }
        if block_interval == 0 {
            return Err(ProposerError::Config("block_interval must not be zero".into()));
        }

        let count = block_interval / intermediate_block_interval;
        (1..=count)
            .map(|i| {
                starting_block_number
                    .checked_add(i.checked_mul(intermediate_block_interval).ok_or_else(|| {
                        ProposerError::Internal(
                            "overflow computing intermediate block number".into(),
                        )
                    })?)
                    .ok_or_else(|| {
                        ProposerError::Internal(
                            "overflow computing intermediate block number".into(),
                        )
                    })
            })
            .collect()
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
