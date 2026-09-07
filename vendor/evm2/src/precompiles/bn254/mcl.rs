//! BN254 precompile implementation using herumi/mcl via [`mcl_rust`].

use std::sync::Once;

use mcl_rust::{CurveType, Fp, Fp2, Fr, G1, G2, GT};

use super::{Bn254Ops, FQ_LEN, FQ2_LEN, G1_LEN, SCALAR_LEN};
use crate::precompiles::PrecompileHalt;

unsafe extern "C" {
    fn mclBn_getCurveType() -> i32;
}

pub(crate) struct MclOps;

impl Bn254Ops for MclOps {
    type G1 = G1;
    type G2 = G2;
    type Scalar = Fr;

    #[inline]
    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            // CurveType::SNARK (MCL_BN_SNARK1) is the Ethereum-compatible BN254 (alt_bn128).
            // CurveType::BN254 is a different, older BN254 parameterization.
            assert!(mcl_rust::init(CurveType::SNARK), "mcl BN254 initialization failed");
        });

        let curve = unsafe { mclBn_getCurveType() };
        assert_eq!(curve, CurveType::SNARK as i32, "mcl curve changed after initialization");
    }

    #[inline]
    fn read_g1(input: &[u8]) -> Result<Self::G1, PrecompileHalt> {
        let px = read_fp(&input[..FQ_LEN])?;
        let py = read_fp(&input[FQ_LEN..G1_LEN])?;

        if px.is_zero() && py.is_zero() {
            return Ok(G1::zero());
        }

        let mut p = G1::zero();
        p.x = px;
        p.y = py;
        p.z = Fp::from_int(1);

        if !p.is_valid() {
            return Err(PrecompileHalt::Bn254AffineGFailedToCreate);
        }

        Ok(p)
    }

    #[inline]
    fn encode_g1(point: Self::G1) -> [u8; G1_LEN] {
        if point.is_zero() {
            return [0u8; G1_LEN];
        }

        // Normalize to affine coordinates (z = 1).
        let mut affine = G1::zero();
        G1::normalize(&mut affine, &point);

        let mut output = [0u8; G1_LEN];
        output[..FQ_LEN].copy_from_slice(&encode_fp(&affine.x));
        output[FQ_LEN..].copy_from_slice(&encode_fp(&affine.y));
        output
    }

    #[inline]
    fn read_g2(input: &[u8]) -> Result<Self::G2, PrecompileHalt> {
        let x = read_fp2(&input[..FQ2_LEN])?;
        let y = read_fp2(&input[FQ2_LEN..2 * FQ2_LEN])?;

        if x.is_zero() && y.is_zero() {
            return Ok(G2::zero());
        }

        let mut p = G2::zero();
        p.x = x;
        p.y = y;
        p.z.d[0] = Fp::from_int(1);

        if !p.is_valid() {
            return Err(PrecompileHalt::Bn254AffineGFailedToCreate);
        }

        Ok(p)
    }

    #[inline]
    fn read_scalar(input: &[u8]) -> Self::Scalar {
        assert_eq!(
            input.len(),
            SCALAR_LEN,
            "unexpected scalar length. got {}, expected {SCALAR_LEN}",
            input.len()
        );

        let mut le_bytes = [0u8; SCALAR_LEN];
        le_bytes.copy_from_slice(input);
        le_bytes.reverse();

        let mut fr = Fr::zero();
        fr.set_little_endian_mod(&le_bytes);
        fr
    }

    #[inline]
    fn g1_is_zero(p: &Self::G1) -> bool {
        p.is_zero()
    }

    #[inline]
    fn g2_is_zero(p: &Self::G2) -> bool {
        p.is_zero()
    }

    #[inline]
    fn g1_add(p1: Self::G1, p2: Self::G1) -> Self::G1 {
        &p1 + &p2
    }

    #[inline]
    fn g1_mul(p: Self::G1, s: Self::Scalar) -> Self::G1 {
        let mut result = G1::zero();
        G1::mul(&mut result, &p, &s);
        result
    }

    #[inline]
    fn pairing_check(g1: &[Self::G1], g2: &[Self::G2]) -> bool {
        let mut acc = GT::zero();
        mcl_rust::miller_loop(&mut acc, &g1[0], &g2[0]);

        for i in 1..g1.len() {
            let mut tmp = GT::zero();
            mcl_rust::miller_loop(&mut tmp, &g1[i], &g2[i]);
            acc *= &tmp;
        }

        let mut result = GT::zero();
        mcl_rust::final_exp(&mut result, &acc);

        result.is_one()
    }
}

#[inline]
fn read_fp(input: &[u8]) -> Result<Fp, PrecompileHalt> {
    let mut le_bytes = [0u8; FQ_LEN];
    le_bytes.copy_from_slice(&input[..FQ_LEN]);
    le_bytes.reverse();

    let mut fp = Fp::zero();
    if !fp.set_little_endian(&le_bytes) {
        return Err(PrecompileHalt::Bn254FieldPointNotAMember);
    }
    if encode_fp(&fp).as_slice() != input {
        return Err(PrecompileHalt::Bn254FieldPointNotAMember);
    }
    Ok(fp)
}

#[inline]
fn encode_fp(fp: &Fp) -> [u8; FQ_LEN] {
    let serialized = fp.serialize();
    let mut result = [0u8; FQ_LEN];
    let len = serialized.len().min(FQ_LEN);
    result[..len].copy_from_slice(&serialized[..len]);
    result[..FQ_LEN].reverse();
    result
}

#[inline]
fn read_fp2(input: &[u8]) -> Result<Fp2, PrecompileHalt> {
    let imag = read_fp(&input[..FQ_LEN])?;
    let real = read_fp(&input[FQ_LEN..2 * FQ_LEN])?;
    let mut fp2 = Fp2::zero();
    fp2.d[0] = real;
    fp2.d[1] = imag;
    Ok(fp2)
}
