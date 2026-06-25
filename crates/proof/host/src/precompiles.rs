//! Accelerated precompile runner for the host program.

use alloy_primitives::{Address, Bytes};
use revm::precompile::{self, Precompile};

use crate::{HostError, Result};

/// List of precompiles that are accelerated by the host program.
const ACCELERATED_PRECOMPILES: &[Precompile] = &[
    precompile::secp256k1::ECRECOVER,
    precompile::bn254::pair::ISTANBUL,
    precompile::bls12_381::g1_add::PRECOMPILE,
    precompile::bls12_381::g1_msm::PRECOMPILE,
    precompile::bls12_381::g2_add::PRECOMPILE,
    precompile::bls12_381::g2_msm::PRECOMPILE,
    precompile::bls12_381::map_fp2_to_g2::PRECOMPILE,
    precompile::bls12_381::map_fp_to_g1::PRECOMPILE,
    precompile::bls12_381::pairing::PRECOMPILE,
    precompile::kzg_point_evaluation::POINT_EVALUATION,
];

/// Executes an accelerated precompile on [revm].
pub fn execute<T: Into<Bytes>>(address: Address, input: T, gas: u64) -> Result<Vec<u8>> {
    if let Some(precompile) =
        ACCELERATED_PRECOMPILES.iter().find(|precompile| *precompile.address() == address)
    {
        let input = input.into();
        let output = precompile
            .execute(&input, gas, 0)
            .map_err(|e| HostError::PrecompileExecutionFailed(e.to_string()))?;
        if output.is_success() {
            Ok(output.bytes.into())
        } else {
            Err(HostError::PrecompileExecutionFailed(format!(
                "precompile returned non-success status: {:?}",
                output.status
            )))
        }
    } else {
        Err(HostError::PrecompileNotAccelerated)
    }
}
