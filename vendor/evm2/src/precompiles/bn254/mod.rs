//! BN254 precompiles added in [`EIP-1962`](https://eips.ethereum.org/EIPS/eip-1962)

use alloc::vec::Vec;

use crate::{
    interpreter::GasTracker,
    precompiles::{PrecompileHalt, PrecompileOutput, PrecompileResult},
    utils::{bool_to_bytes32, right_pad},
};

#[cfg_attr(all(feature = "bn", not(feature = "bn254-mcl")), expect(dead_code))]
pub(crate) mod arkworks;

cfg_if::cfg_if! {
    if #[cfg(feature = "bn254-mcl")] {
        pub(crate) mod mcl;
        type ArithmeticOps = mcl::MclOps;
        type PairingOps = arkworks::ArkworksOps;
    } else if #[cfg(feature = "bn")]{
        pub(crate) mod substrate;
        type ArithmeticOps = substrate::SubstrateOps;
        type PairingOps = substrate::SubstrateOps;
    } else {
        type ArithmeticOps = arkworks::ArkworksOps;
        type PairingOps = arkworks::ArkworksOps;
    }
}

pub(crate) trait Bn254Ops {
    type G1;
    type G2;
    type Scalar;

    #[inline]
    fn init() {}

    fn read_g1(input: &[u8]) -> Result<Self::G1, PrecompileHalt>;
    fn encode_g1(point: Self::G1) -> [u8; G1_LEN];
    fn read_g2(input: &[u8]) -> Result<Self::G2, PrecompileHalt>;
    fn read_scalar(input: &[u8]) -> Self::Scalar;
    fn g1_is_zero(p: &Self::G1) -> bool;
    fn g2_is_zero(p: &Self::G2) -> bool;
    fn g1_add(p1: Self::G1, p2: Self::G1) -> Self::G1;
    fn g1_mul(p: Self::G1, s: Self::Scalar) -> Self::G1;
    fn pairing_check(g1: &[Self::G1], g2: &[Self::G2]) -> bool;
}

/// Performs point addition on two G1 points using the selected backend.
#[inline]
pub(crate) fn g1_point_add(p1_bytes: &[u8], p2_bytes: &[u8]) -> Result<[u8; 64], PrecompileHalt> {
    ArithmeticOps::init();
    let p1 = ArithmeticOps::read_g1(p1_bytes)?;
    let p2 = ArithmeticOps::read_g1(p2_bytes)?;
    Ok(ArithmeticOps::encode_g1(ArithmeticOps::g1_add(p1, p2)))
}

/// Performs a G1 scalar multiplication using the selected backend.
#[inline]
pub(crate) fn g1_point_mul(
    point_bytes: &[u8],
    fr_bytes: &[u8],
) -> Result<[u8; 64], PrecompileHalt> {
    ArithmeticOps::init();
    let p = ArithmeticOps::read_g1(point_bytes)?;
    let fr = ArithmeticOps::read_scalar(fr_bytes);
    Ok(ArithmeticOps::encode_g1(ArithmeticOps::g1_mul(p, fr)))
}

/// Performs a pairing check on a list of G1 and G2 point pairs using the selected backend.
#[inline]
pub(crate) fn pairing_check(pairs: &[(&[u8], &[u8])]) -> Result<bool, PrecompileHalt> {
    PairingOps::init();
    let mut g1_points = Vec::with_capacity(pairs.len());
    let mut g2_points = Vec::with_capacity(pairs.len());

    for (g1_bytes, g2_bytes) in pairs {
        let g1 = PairingOps::read_g1(g1_bytes)?;
        let g2 = PairingOps::read_g2(g2_bytes)?;

        // Skip pairs where either point is at infinity
        if !PairingOps::g1_is_zero(&g1) && !PairingOps::g2_is_zero(&g2) {
            g1_points.push(g1);
            g2_points.push(g2);
        }
    }

    if g1_points.is_empty() {
        return Ok(true);
    }

    Ok(PairingOps::pairing_check(&g1_points, &g2_points))
}

/// BN254 point addition precompile entrypoints.
pub mod add {
    use super::*;

    pub(crate) const ISTANBUL_ADD_GAS_COST: u64 = 150;
    pub(crate) const BYZANTIUM_ADD_GAS_COST: u64 = 500;

    /// Runs the Istanbul BN254 point addition precompile.
    pub fn run_istanbul(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
        run(input, ISTANBUL_ADD_GAS_COST, gas)
    }

    /// Runs the Byzantium BN254 point addition precompile.
    pub fn run_byzantium(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
        run(input, BYZANTIUM_ADD_GAS_COST, gas)
    }

    pub(crate) fn run(input: &[u8], gas_cost: u64, gas: &mut GasTracker) -> PrecompileResult {
        super::run_add(input, gas_cost, gas)
    }
}

/// BN254 scalar multiplication precompile entrypoints.
pub mod mul {
    use super::*;

    pub(crate) const ISTANBUL_MUL_GAS_COST: u64 = 6_000;
    pub(crate) const BYZANTIUM_MUL_GAS_COST: u64 = 40_000;

    /// Runs the Istanbul BN254 scalar multiplication precompile.
    pub fn run_istanbul(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
        run(input, ISTANBUL_MUL_GAS_COST, gas)
    }

    /// Runs the Byzantium BN254 scalar multiplication precompile.
    pub fn run_byzantium(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
        run(input, BYZANTIUM_MUL_GAS_COST, gas)
    }

    pub(crate) fn run(input: &[u8], gas_cost: u64, gas: &mut GasTracker) -> PrecompileResult {
        super::run_mul(input, gas_cost, gas)
    }
}

/// BN254 pairing precompile entrypoints.
pub mod pair {
    use super::*;

    pub(crate) const ISTANBUL_PAIR_PER_POINT: u64 = 34_000;
    pub(crate) const ISTANBUL_PAIR_BASE: u64 = 45_000;
    pub(crate) const BYZANTIUM_PAIR_PER_POINT: u64 = 80_000;
    pub(crate) const BYZANTIUM_PAIR_BASE: u64 = 100_000;

    /// Runs the Istanbul BN254 pairing precompile.
    pub fn run_istanbul(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
        run(input, ISTANBUL_PAIR_PER_POINT, ISTANBUL_PAIR_BASE, gas)
    }

    /// Runs the Byzantium BN254 pairing precompile.
    pub fn run_byzantium(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
        run(input, BYZANTIUM_PAIR_PER_POINT, BYZANTIUM_PAIR_BASE, gas)
    }

    pub(crate) fn run(
        input: &[u8],
        pair_per_point_cost: u64,
        pair_base_cost: u64,
        gas: &mut GasTracker,
    ) -> PrecompileResult {
        super::run_pair(input, pair_per_point_cost, pair_base_cost, gas)
    }
}

/// FQ_LEN specifies the number of bytes needed to represent an
/// Fq element. This is an element in the base field of BN254.
///
/// Note: The base field is used to define G1 and G2 elements.
const FQ_LEN: usize = 32;

/// SCALAR_LEN specifies the number of bytes needed to represent an Fr element.
/// This is an element in the scalar field of BN254.
const SCALAR_LEN: usize = 32;

/// FQ2_LEN specifies the number of bytes needed to represent an
/// Fq^2 element.
///
/// Note: This is the quadratic extension of Fq, and by definition
/// means we need 2 Fq elements.
const FQ2_LEN: usize = 2 * FQ_LEN;

/// G1_LEN specifies the number of bytes needed to represent a G1 element.
///
/// Note: A G1 element contains 2 Fq elements.
const G1_LEN: usize = 2 * FQ_LEN;
/// G2_LEN specifies the number of bytes needed to represent a G2 element.
///
/// Note: A G2 element contains 2 Fq^2 elements.
const G2_LEN: usize = 2 * FQ2_LEN;

/// Input length for the add operation.
/// `ADD` takes two uncompressed G1 points (64 bytes each).
pub(crate) const ADD_INPUT_LEN: usize = 2 * G1_LEN;

/// Input length for the multiplication operation.
/// `MUL` takes an uncompressed G1 point (64 bytes) and scalar (32 bytes).
pub(crate) const MUL_INPUT_LEN: usize = G1_LEN + SCALAR_LEN;

/// Pair element length.
/// `PAIR` elements are composed of an uncompressed G1 point (64 bytes) and an uncompressed G2 point
/// (128 bytes).
pub(crate) const PAIR_ELEMENT_LEN: usize = G1_LEN + G2_LEN;

/// Run the Bn254 add precompile
pub fn run_add(input: &[u8], gas_cost: u64, gas: &mut GasTracker) -> PrecompileResult {
    gas.spend(gas_cost)?;

    let input = right_pad::<ADD_INPUT_LEN>(input);

    let p1_bytes = &input[..G1_LEN];
    let p2_bytes = &input[G1_LEN..];
    let output = crate::precompiles::crypto().bn254_g1_add(p1_bytes, p2_bytes)?;

    Ok(PrecompileOutput::new(output.into()))
}

/// Run the Bn254 mul precompile
pub fn run_mul(input: &[u8], gas_cost: u64, gas: &mut GasTracker) -> PrecompileResult {
    gas.spend(gas_cost)?;

    let input = right_pad::<MUL_INPUT_LEN>(input);

    let point_bytes = &input[..G1_LEN];
    let scalar_bytes = &input[G1_LEN..G1_LEN + SCALAR_LEN];
    let output = crate::precompiles::crypto().bn254_g1_mul(point_bytes, scalar_bytes)?;

    Ok(PrecompileOutput::new(output.into()))
}

/// Run the Bn254 pair precompile
pub fn run_pair(
    input: &[u8],
    pair_per_point_cost: u64,
    pair_base_cost: u64,
    gas: &mut GasTracker,
) -> PrecompileResult {
    let gas_used = (input.len() / PAIR_ELEMENT_LEN) as u64 * pair_per_point_cost + pair_base_cost;
    gas.spend(gas_used)?;

    if !input.len().is_multiple_of(PAIR_ELEMENT_LEN) {
        return Err(PrecompileHalt::Bn254PairLength.into());
    }

    let elements = input.len() / PAIR_ELEMENT_LEN;

    let mut points = Vec::with_capacity(elements);

    for idx in 0..elements {
        // Offset to the start of the pairing element at index `idx` in the byte slice
        let start = idx * PAIR_ELEMENT_LEN;
        let g1_start = start;
        // Offset to the start of the G2 element in the pairing element
        // This is where G1 ends.
        let g2_start = start + G1_LEN;

        // Get G1 and G2 points from the input
        let encoded_g1_element = &input[g1_start..g2_start];
        let encoded_g2_element = &input[g2_start..g2_start + G2_LEN];
        points.push((encoded_g1_element, encoded_g2_element));
    }

    let pairing_result = crate::precompiles::crypto().bn254_pairing_check(&points)?;
    Ok(PrecompileOutput::new(bool_to_bytes32(pairing_result)))
}

#[cfg(test)]
mod tests {
    use core::assert_matches;

    use alloy_primitives::hex;

    use super::*;
    use crate::precompiles::{
        PrecompileError, PrecompileHalt,
        bn254::{
            add::BYZANTIUM_ADD_GAS_COST,
            mul::BYZANTIUM_MUL_GAS_COST,
            pair::{BYZANTIUM_PAIR_BASE, BYZANTIUM_PAIR_PER_POINT},
        },
    };

    #[test]
    fn test_bn254_add() {
        let input = hex::decode(
            "\
             18b18acfb4c2c30276db5411368e7185b311dd124691610c5d3b74034e093dc9\
             063c909c4720840cb5134cb9f59fa749755796819658d32efc0d288198f37266\
             07c2b7f58a84bd6145f00c9c2bc0bb1a187f20ff2c92963a88019e7c6a014eed\
             06614e20c147e940f2d70da3f74c9a17df361706a4485c742bd6788478fa17d7",
        )
        .unwrap();
        let expected = hex::decode(
            "\
            2243525c5efd4b9c3d3c45ac0ca3fe4dd85e830a4ce6b65fa1eeaee202839703\
            301d1d33be6da8e509df21cc35964723180eed7532537db9ae5e7d48f195c915",
        )
        .unwrap();

        let outcome = run_add(&input, BYZANTIUM_ADD_GAS_COST, &mut GasTracker::new(500)).unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Zero sum test
        let input = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();
        let expected = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let outcome = run_add(&input, BYZANTIUM_ADD_GAS_COST, &mut GasTracker::new(500)).unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Out of gas test
        let input = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let res = run_add(&input, BYZANTIUM_ADD_GAS_COST, &mut GasTracker::new(499));

        assert_matches!(res, Err(PrecompileError::Halt(PrecompileHalt::OutOfGas)));

        // No input test
        let input = [0u8; 0];
        let expected = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let outcome = run_add(&input, BYZANTIUM_ADD_GAS_COST, &mut GasTracker::new(500)).unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Point not on curve fail
        let input = hex::decode(
            "\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();

        let res = run_add(&input, BYZANTIUM_ADD_GAS_COST, &mut GasTracker::new(500));
        assert_matches!(
            res,
            Err(PrecompileError::Halt(PrecompileHalt::Bn254AffineGFailedToCreate)),
        );

        // Short input is right-padded. This makes the first field element non-canonical.
        let res = run_add(&[0x40], BYZANTIUM_ADD_GAS_COST, &mut GasTracker::new(500));
        assert_matches!(res, Err(PrecompileError::Halt(PrecompileHalt::Bn254FieldPointNotAMember)),);

        let input = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000001\
            30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45\
            0000000000000000000000000000000000000000000000000000000000000001\
            30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45",
        )
        .unwrap();
        let expected = hex::decode(
            "\
            030644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd3\
            1a76dae6d3272396d0cbe61fced2bc532edac647851e3ac53ce1cc9c7e645a83",
        )
        .unwrap();

        let outcome = run_add(&input, BYZANTIUM_ADD_GAS_COST, &mut GasTracker::new(500)).unwrap();
        assert_eq!(outcome.bytes(), expected);
    }

    #[test]
    fn test_bn254_mul() {
        let input = hex::decode(
            "\
            2bd3e6d0f3b142924f5ca7b49ce5b9d54c4703d7ae5648e61d02268b1a0a9fb7\
            21611ce0a6af85915e2f1d70300909ce2e49dfad4a4619c8390cae66cefdb204\
            00000000000000000000000000000000000000000000000011138ce750fa15c2",
        )
        .unwrap();
        let expected = hex::decode(
            "\
            070a8d6a982153cae4be29d434e8faef8a47b274a053f5a4ee2a6c9c13c31e5c\
            031b8ce914eba3a9ffb989f9cdd5b0f01943074bf4f0f315690ec3cec6981afc",
        )
        .unwrap();

        let outcome =
            run_mul(&input, BYZANTIUM_MUL_GAS_COST, &mut GasTracker::new(40_000)).unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Out of gas test
        let input = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0200000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let res = run_mul(&input, BYZANTIUM_MUL_GAS_COST, &mut GasTracker::new(39_999));
        assert_matches!(res, Err(PrecompileError::Halt(PrecompileHalt::OutOfGas)));

        // Zero multiplication test
        let input = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0200000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();
        let expected = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let outcome =
            run_mul(&input, BYZANTIUM_MUL_GAS_COST, &mut GasTracker::new(40_000)).unwrap();
        assert_eq!(outcome.bytes(), expected);

        // No input test
        let input = [0u8; 0];
        let expected = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let outcome =
            run_mul(&input, BYZANTIUM_MUL_GAS_COST, &mut GasTracker::new(40_000)).unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Point not on curve fail
        let input = hex::decode(
            "\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            0f00000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let res = run_mul(&input, BYZANTIUM_MUL_GAS_COST, &mut GasTracker::new(40_000));
        assert_matches!(
            res,
            Err(PrecompileError::Halt(PrecompileHalt::Bn254AffineGFailedToCreate)),
        );

        // Short input is right-padded. This makes the point x-coordinate non-canonical.
        let res = run_mul(&[0x40], BYZANTIUM_MUL_GAS_COST, &mut GasTracker::new(40_000));
        assert_matches!(res, Err(PrecompileError::Halt(PrecompileHalt::Bn254FieldPointNotAMember)),);

        let input = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000001\
            30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45\
            0000000000000000000000000000000000000000000000000000000000000002",
        )
        .unwrap();
        let expected = hex::decode(
            "\
            030644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd3\
            1a76dae6d3272396d0cbe61fced2bc532edac647851e3ac53ce1cc9c7e645a83",
        )
        .unwrap();

        let outcome =
            run_mul(&input, BYZANTIUM_MUL_GAS_COST, &mut GasTracker::new(40_000)).unwrap();
        assert_eq!(outcome.bytes(), expected);
    }

    #[test]
    fn test_bn254_pair() {
        let input = hex::decode(
            "\
            1c76476f4def4bb94541d57ebba1193381ffa7aa76ada664dd31c16024c43f59\
            3034dd2920f673e204fee2811c678745fc819b55d3e9d294e45c9b03a76aef41\
            209dd15ebff5d46c4bd888e51a93cf99a7329636c63514396b4a452003a35bf7\
            04bf11ca01483bfa8b34b43561848d28905960114c8ac04049af4b6315a41678\
            2bb8324af6cfc93537a2ad1a445cfd0ca2a71acd7ac41fadbf933c2a51be344d\
            120a2a4cf30c1bf9845f20c6fe39e07ea2cce61f0c9bb048165fe5e4de877550\
            111e129f1cf1097710d41c4ac70fcdfa5ba2023c6ff1cbeac322de49d1b6df7c\
            2032c61a830e3c17286de9462bf242fca2883585b93870a73853face6a6bf411\
            198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2\
            1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed\
            090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b\
            12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa",
        )
        .unwrap();
        let expected =
            hex::decode("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        let outcome = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(260_000),
        )
        .unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Out of gas test
        let input = hex::decode(
            "\
            1c76476f4def4bb94541d57ebba1193381ffa7aa76ada664dd31c16024c43f59\
            3034dd2920f673e204fee2811c678745fc819b55d3e9d294e45c9b03a76aef41\
            209dd15ebff5d46c4bd888e51a93cf99a7329636c63514396b4a452003a35bf7\
            04bf11ca01483bfa8b34b43561848d28905960114c8ac04049af4b6315a41678\
            2bb8324af6cfc93537a2ad1a445cfd0ca2a71acd7ac41fadbf933c2a51be344d\
            120a2a4cf30c1bf9845f20c6fe39e07ea2cce61f0c9bb048165fe5e4de877550\
            111e129f1cf1097710d41c4ac70fcdfa5ba2023c6ff1cbeac322de49d1b6df7c\
            2032c61a830e3c17286de9462bf242fca2883585b93870a73853face6a6bf411\
            198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2\
            1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed\
            090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b\
            12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa",
        )
        .unwrap();

        let res = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(259_999),
        );
        assert_matches!(res, Err(PrecompileError::Halt(PrecompileHalt::OutOfGas)));

        // No input test
        let input = [0u8; 0];
        let expected =
            hex::decode("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        let outcome = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(260_000),
        )
        .unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Point not on curve fail
        let input = hex::decode(
            "\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();

        let res = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(260_000),
        );
        assert_matches!(
            res,
            Err(PrecompileError::Halt(PrecompileHalt::Bn254AffineGFailedToCreate)),
        );

        let mut input = [0u8; PAIR_ELEMENT_LEN];
        input[0] = 0x40;
        let res = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(180_000),
        );
        assert_matches!(res, Err(PrecompileError::Halt(PrecompileHalt::Bn254FieldPointNotAMember)));

        // Invalid input length
        let input = hex::decode(
            "\
            1111111111111111111111111111111111111111111111111111111111111111\
            1111111111111111111111111111111111111111111111111111111111111111\
            111111111111111111111111111111\
        ",
        )
        .unwrap();

        let res = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(260_000),
        );
        assert_matches!(res, Err(PrecompileError::Halt(PrecompileHalt::Bn254PairLength)));

        // Test with point at infinity - should return true (identity element)
        // G1 point at infinity (0,0) followed by a valid G2 point
        let input = hex::decode(
            "\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            209dd15ebff5d46c4bd888e51a93cf99a7329636c63514396b4a452003a35bf7\
            04bf11ca01483bfa8b34b43561848d28905960114c8ac04049af4b6315a41678\
            2bb8324af6cfc93537a2ad1a445cfd0ca2a71acd7ac41fadbf933c2a51be344d\
            120a2a4cf30c1bf9845f20c6fe39e07ea2cce61f0c9bb048165fe5e4de877550",
        )
        .unwrap();
        let expected =
            hex::decode("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        let outcome = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(260_000),
        )
        .unwrap();
        assert_eq!(outcome.bytes(), expected);

        // Test with G2 point at infinity - should also return true
        // Valid G1 point followed by G2 point at infinity (0,0,0,0)
        let input = hex::decode(
            "\
            1c76476f4def4bb94541d57ebba1193381ffa7aa76ada664dd31c16024c43f59\
            3034dd2920f673e204fee2811c678745fc819b55d3e9d294e45c9b03a76aef41\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000\
            0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();

        let outcome = run_pair(
            &input,
            BYZANTIUM_PAIR_PER_POINT,
            BYZANTIUM_PAIR_BASE,
            &mut GasTracker::new(260_000),
        )
        .unwrap();
        assert_eq!(outcome.bytes(), expected);
    }
}
