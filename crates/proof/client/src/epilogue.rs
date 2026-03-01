//! The epilogue phase - validation of the computed output root.

use alloc::boxed::Box;

use alloy_primitives::B256;
use base_protocol::L2BlockInfo;
use tracing::info;

use crate::error::FaultProofProgramError;

/// The result of executing the fault proof program.
///
/// Contains the computed output root and claimed output root for validation.
#[derive(Debug)]
pub struct Epilogue {
    /// The L2 block that was derived.
    pub safe_head: L2BlockInfo,

    /// The computed output root from execution.
    pub output_root: B256,

    /// The claimed output root to validate against.
    pub claimed_output_root: B256,
}

impl Epilogue {
    /// Validates that the computed output root matches the claimed output root.
    ///
    /// # Errors
    ///
    /// Returns an error if the computed and claimed output roots do not match.
    pub fn validate(self) -> Result<(), Box<FaultProofProgramError>> {
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
