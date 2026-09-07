use alloc::vec::Vec;

use substrate_bn::{AffineG1, AffineG2, Fq, Fq2, G1, G2, Group, Gt};

use super::{Bn254Ops, FQ_LEN, FQ2_LEN, G1_LEN, SCALAR_LEN};
use crate::precompiles::PrecompileHalt;

pub(crate) struct SubstrateOps;

impl Bn254Ops for SubstrateOps {
    type G1 = G1;
    type G2 = G2;
    type Scalar = substrate_bn::Fr;

    #[inline]
    fn read_g1(input: &[u8]) -> Result<Self::G1, PrecompileHalt> {
        read_g1_point(input)
    }

    #[inline]
    fn encode_g1(point: Self::G1) -> [u8; G1_LEN] {
        encode_g1_point(point)
    }

    #[inline]
    fn read_g2(input: &[u8]) -> Result<Self::G2, PrecompileHalt> {
        read_g2_point(input)
    }

    #[inline]
    fn read_scalar(input: &[u8]) -> Self::Scalar {
        read_scalar(input)
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
        p1 + p2
    }

    #[inline]
    fn g1_mul(p: Self::G1, s: Self::Scalar) -> Self::G1 {
        p * s
    }

    #[inline]
    fn pairing_check(g1: &[Self::G1], g2: &[Self::G2]) -> bool {
        let pairs: Vec<(G1, G2)> = g1.iter().copied().zip(g2.iter().copied()).collect();
        substrate_bn::pairing_batch(&pairs) == Gt::one()
    }
}

/// Reads a single `Fq` field element from the input slice.
///
/// Takes a byte slice and attempts to interpret the first 32 bytes as an
/// elliptic curve field element. Returns an error if the bytes do not form
/// a valid field element.
///
/// # Panics
///
/// Panics if the input is not at least 32 bytes long.
#[inline]
fn read_fq(input: &[u8]) -> Result<Fq, PrecompileHalt> {
    Fq::from_slice(&input[..FQ_LEN]).map_err(|_| PrecompileHalt::Bn254FieldPointNotAMember)
}

/// Reads a Fq2 (quadratic extension field element) from the input slice.
///
/// Parses two consecutive Fq field elements as the real and imaginary parts
/// of an Fq2 element.
/// The second component is parsed before the first, ie if a we represent an
/// element in Fq2 as (x,y) -- `y` is parsed before `x`
///
/// # Panics
///
/// Panics if the input is not at least 64 bytes long.
#[inline]
fn read_fq2(input: &[u8]) -> Result<Fq2, PrecompileHalt> {
    let y = read_fq(&input[..FQ_LEN])?;
    let x = read_fq(&input[FQ_LEN..2 * FQ_LEN])?;
    Ok(Fq2::new(x, y))
}

/// Creates a new `G1` point from the given `x` and `y` coordinates.
///
/// Constructs a point on the G1 curve from its affine coordinates.
///
/// Note: The point at infinity which is represented as (0,0) is
/// handled specifically because `AffineG1` is not capable of
/// representing such a point.
/// In particular, when we convert from `AffineG1` to `G1`, the point
/// will be (0,0,1) instead of (0,1,0)
#[inline]
fn new_g1_point(px: Fq, py: Fq) -> Result<G1, PrecompileHalt> {
    if px == Fq::zero() && py == Fq::zero() {
        Ok(G1::zero())
    } else {
        AffineG1::new(px, py)
            .map(Into::into)
            .map_err(|_| PrecompileHalt::Bn254AffineGFailedToCreate)
    }
}

/// Creates a new `G2` point from the given Fq2 coordinates.
///
/// G2 points in BN254 are defined over a quadratic extension field Fq2.
/// This function takes two Fq2 elements representing the x and y coordinates
/// and creates a G2 point.
///
/// Note: The point at infinity which is represented as (0,0) is
/// handled specifically because `AffineG2` is not capable of
/// representing such a point.
/// In particular, when we convert from `AffineG2` to `G2`, the point
/// will be (0,0,1) instead of (0,1,0)
#[inline]
fn new_g2_point(x: Fq2, y: Fq2) -> Result<G2, PrecompileHalt> {
    let point = if x.is_zero() && y.is_zero() {
        G2::zero()
    } else {
        G2::from(AffineG2::new(x, y).map_err(|_| PrecompileHalt::Bn254AffineGFailedToCreate)?)
    };

    Ok(point)
}

/// Reads a G1 point from the input slice.
///
/// Parses a G1 point from a byte slice by reading two consecutive field elements
/// representing the x and y coordinates.
///
/// # Panics
///
/// Panics if the input is not at least 64 bytes long.
#[inline]
pub(super) fn read_g1_point(input: &[u8]) -> Result<G1, PrecompileHalt> {
    let px = read_fq(&input[0..FQ_LEN])?;
    let py = read_fq(&input[FQ_LEN..2 * FQ_LEN])?;
    new_g1_point(px, py)
}

/// Encodes a G1 point into a byte array.
///
/// Converts a G1 point in Jacobian coordinates to affine coordinates and
/// serializes the x and y coordinates as big-endian byte arrays.
///
/// Note: If the point is the point at infinity, this function returns
/// all zeroes.
#[inline]
pub(super) fn encode_g1_point(point: G1) -> [u8; G1_LEN] {
    let mut output = [0u8; G1_LEN];

    if let Some(point_affine) = AffineG1::from_jacobian(point) {
        point_affine.x().to_big_endian(&mut output[..FQ_LEN]).unwrap();
        point_affine.y().to_big_endian(&mut output[FQ_LEN..]).unwrap();
    }

    output
}

/// Reads a G2 point from the input slice.
///
/// Parses a G2 point from a byte slice by reading four consecutive Fq field elements
/// representing the two Fq2 coordinates (x and y) of the G2 point.
///
/// # Panics
///
/// Panics if the input is not at least 128 bytes long.
#[inline]
pub(super) fn read_g2_point(input: &[u8]) -> Result<G2, PrecompileHalt> {
    let x = read_fq2(&input[0..FQ2_LEN])?;
    let y = read_fq2(&input[FQ2_LEN..2 * FQ2_LEN])?;
    new_g2_point(x, y)
}

/// Reads a scalar from the input slice
///
/// Note: The scalar does not need to be canonical.
///
/// # Panics
///
/// If `input.len()` is not equal to [`SCALAR_LEN`].
#[inline]
pub(super) fn read_scalar(input: &[u8]) -> substrate_bn::Fr {
    assert_eq!(
        input.len(),
        SCALAR_LEN,
        "unexpected scalar length. got {}, expected {SCALAR_LEN}",
        input.len()
    );
    // `Fr::from_slice` can only fail when the length is not `SCALAR_LEN`.
    substrate_bn::Fr::from_slice(input).unwrap()
}
