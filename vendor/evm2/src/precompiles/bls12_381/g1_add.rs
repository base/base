//! BLS12-381 G1 add precompile. More details in [`run`]

use super::utils::{pad_g1_point, remove_g1_padding};
use crate::{
    interpreter::GasTracker,
    precompiles::{
        PrecompileHalt, PrecompileOutput, PrecompileResult,
        bls12_381_const::{G1_ADD_BASE_GAS_FEE, G1_ADD_INPUT_LENGTH, PADDED_G1_LENGTH},
    },
};

/// G1 addition call expects `256` bytes as an input that is interpreted as byte
/// concatenation of two G1 points (`128` bytes each).
/// Output is an encoding of addition operation result - single G1 point (`128`
/// bytes).
/// See also: <https://eips.ethereum.org/EIPS/eip-2537#abi-for-g1-addition>
pub fn run(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
    gas.spend(G1_ADD_BASE_GAS_FEE)?;

    if input.len() != G1_ADD_INPUT_LENGTH {
        return Err(PrecompileHalt::Bls12381G1AddInputLength.into());
    }

    // Extract coordinates from padded input
    let [a_x, a_y] = remove_g1_padding(&input[..PADDED_G1_LENGTH])?;
    let [b_x, b_y] = remove_g1_padding(&input[PADDED_G1_LENGTH..])?;

    let a = (*a_x, *a_y);
    let b = (*b_x, *b_y);

    let unpadded_result = crate::precompiles::crypto().bls12_381_g1_add(a, b)?;

    // Pad the result for EVM compatibility
    let padded_result = pad_g1_point(&unpadded_result);

    Ok(PrecompileOutput::new(padded_result.into()))
}
