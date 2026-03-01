//! Validation logic for claims.

use alloy_primitives::B256;
use base_protocol::OpAttributesWithParent;
use thiserror::Error;

/// Error type for claim validation failures.
#[derive(Error, Debug)]
pub enum ClaimValidationError {
    /// Derived block number does not match claimed block number.
    #[error("derived block {derived} does not match claimed {claimed}")]
    BlockNumberMismatch {
        /// The block number produced by the derivation pipeline.
        derived: u64,
        /// The block number asserted in the claim.
        claimed: u64,
    },
    /// Computed output root does not match claimed output root.
    #[error("computed output root {computed} does not match claimed {claimed}")]
    OutputRootMismatch {
        /// The output root computed from execution.
        computed: B256,
        /// The output root asserted in the claim.
        claimed: B256,
    },
}

/// Validator for block claims.
#[derive(Debug, Clone, Copy)]
pub struct ClaimValidator;

impl ClaimValidator {
    /// Validates that the claimed L2 block number matches the derived block number.
    pub const fn validate_block_number(
        attributes: &OpAttributesWithParent,
        claimed_l2_block_number: u64,
    ) -> Result<(), ClaimValidationError> {
        let derived = attributes.block_number();
        if derived != claimed_l2_block_number {
            return Err(ClaimValidationError::BlockNumberMismatch {
                derived,
                claimed: claimed_l2_block_number,
            });
        }
        Ok(())
    }

    /// Validates that the computed output root matches the claimed output root.
    pub fn validate_output_root(computed: B256, claimed: B256) -> Result<(), ClaimValidationError> {
        if computed != claimed {
            return Err(ClaimValidationError::OutputRootMismatch { computed, claimed });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip1898::BlockNumHash;
    use base_alloy_rpc_types_engine::OpPayloadAttributes;
    use base_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
    use rstest::rstest;

    use super::*;

    fn attrs(parent: u64) -> OpAttributesWithParent {
        let parent = L2BlockInfo {
            block_info: BlockInfo::new(B256::ZERO, parent, B256::ZERO, 0),
            l1_origin: BlockNumHash::default(),
            seq_num: 0,
        };
        OpAttributesWithParent::new(OpPayloadAttributes::default(), parent, None, false)
    }

    #[rstest]
    #[case(10, 11, true)]
    #[case(10, 12, false)]
    fn test_validate_block_number(#[case] parent: u64, #[case] claimed: u64, #[case] ok: bool) {
        assert_eq!(ClaimValidator::validate_block_number(&attrs(parent), claimed).is_ok(), ok);
    }

    #[rstest]
    #[case([1u8; 32], [1u8; 32], true)]
    #[case([1u8; 32], [2u8; 32], false)]
    fn test_validate_output_root(
        #[case] computed: [u8; 32],
        #[case] claimed: [u8; 32],
        #[case] ok: bool,
    ) {
        assert_eq!(
            ClaimValidator::validate_output_root(computed.into(), claimed.into()).is_ok(),
            ok
        );
    }
}
