//! BLS12-381 G1 msm precompile. More details in [`run`]

use crate::{
    interpreter::GasTracker,
    precompiles::{
        PrecompileHalt, PrecompileOutput, PrecompileResult,
        bls12_381::{
            G1Point,
            utils::{pad_g1_point, remove_g1_padding},
        },
        bls12_381_const::{
            DISCOUNT_TABLE_G1_MSM, G1_MSM_BASE_GAS_FEE, G1_MSM_INPUT_LENGTH, PADDED_G1_LENGTH,
            SCALAR_LENGTH,
        },
        bls12_381_utils::msm_required_gas,
    },
};

/// Implements EIP-2537 G1MSM precompile.
/// G1 multi-scalar-multiplication call expects `160*k` bytes as an input that is interpreted
/// as byte concatenation of `k` slices each of them being a byte concatenation
/// of encoding of G1 point (`128` bytes) and encoding of a scalar value (`32`
/// bytes).
/// Output is an encoding of multi-scalar-multiplication operation result - single G1
/// point (`128` bytes).
/// See also: <https://eips.ethereum.org/EIPS/eip-2537#abi-for-g1-multiexponentiation>
pub fn run(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
    let input_len = input.len();
    if input_len == 0 || !input_len.is_multiple_of(G1_MSM_INPUT_LENGTH) {
        return Err(PrecompileHalt::Bls12381G1MsmInputLength.into());
    }

    let k = input_len / G1_MSM_INPUT_LENGTH;
    let required_gas = msm_required_gas(k, &DISCOUNT_TABLE_G1_MSM, G1_MSM_BASE_GAS_FEE);
    gas.spend(required_gas)?;

    let mut valid_pairs_iter = (0..k).map(|i| {
        let start = i * G1_MSM_INPUT_LENGTH;
        let padded_g1 = &input[start..start + PADDED_G1_LENGTH];
        let scalar_bytes = &input[start + PADDED_G1_LENGTH..start + G1_MSM_INPUT_LENGTH];

        // Remove padding from G1 point - this validates padding format
        let [x, y] = remove_g1_padding(padded_g1)?;
        let scalar_array: [u8; SCALAR_LENGTH] = scalar_bytes.try_into().unwrap();

        let point: G1Point = (*x, *y);
        Ok((point, scalar_array))
    });

    let unpadded_result = crate::precompiles::crypto().bls12_381_g1_msm(&mut valid_pairs_iter)?;

    // Pad the result for EVM compatibility
    let padded_result = pad_g1_point(&unpadded_result);

    Ok(PrecompileOutput::new(padded_result.into()))
}

#[cfg(test)]
mod test {
    use alloy_primitives::{Bytes, hex};

    use super::*;

    #[test]
    fn bls_g1multiexp_g1_not_on_curve_but_in_subgroup() {
        let input = Bytes::from(hex!(
            "000000000000000000000000000000000a2833e497b38ee3ca5c62828bf4887a9f940c9e426c7890a759c20f248c23a7210d2432f4c98a514e524b5184a0ddac00000000000000000000000000000000150772d56bf9509469f9ebcd6e47570429fd31b0e262b66d512e245c38ec37255529f2271fd70066473e393a8bead0c30000000000000000000000000000000000000000000000000000000000000000"
        ));
        let fail = run(&input, &mut GasTracker::new(G1_MSM_BASE_GAS_FEE));
        assert_eq!(
            fail.err().and_then(|e| e.as_halt().cloned()),
            Some(PrecompileHalt::Bls12381G1NotOnCurve)
        );
    }
}
