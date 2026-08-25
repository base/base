use alloc::boxed::Box;

use alloy_primitives::B256;
use base_protocol::L2BlockInfo;

use crate::FaultProofProgramError;

/// The result of executing the proof program.
#[derive(Debug)]
pub struct Epilogue {
    /// The L2 block that was derived.
    pub safe_head: L2BlockInfo,
    /// The claimed L2 block number.
    pub claimed_l2_block_number: u64,
    /// The computed output root from execution.
    pub output_root: B256,
    /// The claimed output root to validate against.
    pub claimed_output_root: B256,
}

impl Epilogue {
    /// Validates that the derived block and output root match the claim.
    ///
    /// # Errors
    ///
    /// Returns an error if the derived block number or computed output root does not match the
    /// claim.
    pub fn validate(self) -> Result<(), Box<FaultProofProgramError>> {
        if self.safe_head.block_info.number != self.claimed_l2_block_number {
            return Err(Box::new(FaultProofProgramError::InvalidClaimBlock {
                derived: self.safe_head.block_info.number,
                claimed: self.claimed_l2_block_number,
            }));
        }
        if self.output_root != self.claimed_output_root {
            return Err(Box::new(FaultProofProgramError::InvalidClaim {
                computed: self.output_root,
                claimed: self.claimed_output_root,
            }));
        }
        info!(
            block_number = self.safe_head.block_info.number,
            output_root = %self.output_root,
            "validated output root"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_protocol::L2BlockInfo;
    use rstest::rstest;

    use super::Epilogue;
    use crate::FaultProofProgramError;

    fn epilogue(computed: B256, claimed: B256) -> Epilogue {
        Epilogue {
            safe_head: L2BlockInfo::default(),
            claimed_l2_block_number: 0,
            output_root: computed,
            claimed_output_root: claimed,
        }
    }

    #[rstest]
    #[case([1u8; 32], [1u8; 32], true)]
    #[case([1u8; 32], [2u8; 32], false)]
    fn test_validate_matching_roots(
        #[case] computed: [u8; 32],
        #[case] claimed: [u8; 32],
        #[case] ok: bool,
    ) {
        assert_eq!(epilogue(computed.into(), claimed.into()).validate().is_ok(), ok);
    }

    #[test]
    fn test_validate_mismatch_carries_both_roots() {
        let computed = B256::from([1u8; 32]);
        let claimed = B256::from([2u8; 32]);
        let err = epilogue(computed, claimed).validate().unwrap_err();
        match *err {
            FaultProofProgramError::InvalidClaim { computed: c, claimed: cl } => {
                assert_eq!(c, computed);
                assert_eq!(cl, claimed);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn test_validate_requires_claimed_block() {
        let mut epilogue = epilogue(B256::ZERO, B256::ZERO);
        epilogue.safe_head.block_info.number = 6;
        epilogue.claimed_l2_block_number = 7;

        let error = epilogue.validate().unwrap_err();
        assert!(matches!(
            *error,
            FaultProofProgramError::InvalidClaimBlock { derived: 6, claimed: 7 }
        ));
    }
}
