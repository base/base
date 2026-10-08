//! Vanilla `bls12_381` under the `sp1_bls12_381` crate name, plus the extra
//! `G1Projective::msm_variable_base` method `kzg-rs` 0.2.8 calls.
//!
//! `kzg-rs` depends on `sp1_bls12_381`, which emits SP1 syscalls on
//! `target_os = "zkvm"`. OpenVM shares that target, so we re-export a software
//! curve and wrap the affine/projective types (`#[repr(transparent)]` so
//! `kzg-rs`'s host `build.rs` transmutes keep working).

#![no_std]

extern crate alloc;

use core::ops::{Add, Mul, Neg, Sub};

use subtle::{Choice, CtOption};

pub use bls12_381::{Gt, Scalar};

/// Affine G1 point.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct G1Affine(bls12_381::G1Affine);

/// Projective G1 point.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct G1Projective(bls12_381::G1Projective);

/// Affine G2 point.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct G2Affine(bls12_381::G2Affine);

/// Projective G2 point.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct G2Projective(bls12_381::G2Projective);

/// Prepared G2 point for pairing.
#[derive(Clone, Debug)]
pub struct G2Prepared(bls12_381::G2Prepared);

impl G1Affine {
    /// Identity point.
    pub fn identity() -> Self {
        Self(bls12_381::G1Affine::identity())
    }

    /// Curve generator.
    pub fn generator() -> Self {
        Self(bls12_381::G1Affine::generator())
    }

    /// Decode from compressed bytes.
    pub fn from_compressed(bytes: &[u8; 48]) -> CtOption<Self> {
        bls12_381::G1Affine::from_compressed(bytes).map(Self)
    }

    /// Decode from compressed bytes without subgroup checks.
    pub fn from_compressed_unchecked(bytes: &[u8; 48]) -> CtOption<Self> {
        bls12_381::G1Affine::from_compressed_unchecked(bytes).map(Self)
    }

    /// Returns true if this is the identity.
    pub fn is_identity(&self) -> Choice {
        self.0.is_identity()
    }

    /// Returns true if this point is on the curve.
    pub fn is_on_curve(&self) -> Choice {
        self.0.is_on_curve()
    }

    /// Compress to 48 bytes.
    pub fn to_compressed(&self) -> [u8; 48] {
        self.0.to_compressed()
    }
}

impl G1Projective {
    /// Identity point.
    pub fn identity() -> Self {
        Self(bls12_381::G1Projective::identity())
    }

    /// Curve generator.
    pub fn generator() -> Self {
        Self(bls12_381::G1Projective::generator())
    }

    /// Variable-base MSM. Software fallback for the SP1 helper `kzg-rs` calls.
    pub fn msm_variable_base(points: &[Self], scalars: &[Scalar]) -> Self {
        let mut acc = bls12_381::G1Projective::identity();
        for (point, scalar) in points.iter().zip(scalars) {
            acc += point.0 * scalar;
        }
        Self(acc)
    }
}

impl G2Affine {
    /// Identity point.
    pub fn identity() -> Self {
        Self(bls12_381::G2Affine::identity())
    }

    /// Curve generator.
    pub fn generator() -> Self {
        Self(bls12_381::G2Affine::generator())
    }

    /// Decode from compressed bytes without subgroup checks.
    pub fn from_compressed_unchecked(bytes: &[u8; 96]) -> CtOption<Self> {
        bls12_381::G2Affine::from_compressed_unchecked(bytes).map(Self)
    }
}

impl G2Projective {
    /// Curve generator.
    pub fn generator() -> Self {
        Self(bls12_381::G2Projective::generator())
    }
}

impl From<G2Affine> for G2Prepared {
    fn from(point: G2Affine) -> Self {
        Self(bls12_381::G2Prepared::from(point.0))
    }
}

impl Default for G1Affine {
    fn default() -> Self {
        Self::identity()
    }
}

impl Default for G2Affine {
    fn default() -> Self {
        Self::identity()
    }
}

impl From<G1Affine> for G1Projective {
    fn from(point: G1Affine) -> Self {
        Self(bls12_381::G1Projective::from(point.0))
    }
}

impl From<&G1Affine> for G1Projective {
    fn from(point: &G1Affine) -> Self {
        Self::from(*point)
    }
}

impl From<G1Projective> for G1Affine {
    fn from(point: G1Projective) -> Self {
        Self(bls12_381::G1Affine::from(point.0))
    }
}

impl From<G2Projective> for G2Affine {
    fn from(point: G2Projective) -> Self {
        Self(bls12_381::G2Affine::from(point.0))
    }
}

impl Neg for G1Affine {
    type Output = Self;

    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl Mul<Scalar> for G1Affine {
    type Output = G1Projective;

    fn mul(self, rhs: Scalar) -> G1Projective {
        G1Projective(self.0 * rhs)
    }
}

impl Mul<Scalar> for G1Projective {
    type Output = Self;

    fn mul(self, rhs: Scalar) -> Self {
        Self(self.0 * rhs)
    }
}

impl Mul<Scalar> for G2Affine {
    type Output = G2Projective;

    fn mul(self, rhs: Scalar) -> G2Projective {
        G2Projective(self.0 * rhs)
    }
}

impl Mul<Scalar> for G2Projective {
    type Output = Self;

    fn mul(self, rhs: Scalar) -> Self {
        Self(self.0 * rhs)
    }
}

impl Add for G1Projective {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub<G1Projective> for G1Affine {
    type Output = G1Projective;

    fn sub(self, rhs: G1Projective) -> G1Projective {
        G1Projective(bls12_381::G1Projective::from(self.0) - rhs.0)
    }
}

impl Sub<G2Projective> for G2Affine {
    type Output = G2Projective;

    fn sub(self, rhs: G2Projective) -> G2Projective {
        G2Projective(bls12_381::G2Projective::from(self.0) - rhs.0)
    }
}

impl PartialEq for G1Affine {
    fn eq(&self, other: &Self) -> bool {
        bool::from(self.0.eq(&other.0))
    }
}

impl Eq for G1Affine {}

impl PartialEq for G2Affine {
    fn eq(&self, other: &Self) -> bool {
        bool::from(self.0.eq(&other.0))
    }
}

impl Eq for G2Affine {}

/// Multi-miller loop used by `kzg-rs` pairing verification.
pub fn multi_miller_loop(terms: &[(&G1Affine, &G2Prepared)]) -> bls12_381::MillerLoopResult {
    let mapped: alloc::vec::Vec<(&bls12_381::G1Affine, &bls12_381::G2Prepared)> =
        terms.iter().map(|(g1, g2)| (&g1.0, &g2.0)).collect();
    bls12_381::multi_miller_loop(&mapped)
}
