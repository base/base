//! `bls12_381` under the `sp1_bls12_381` crate name, plus the extra
//! `G1Projective::msm_variable_base` method `kzg-rs` 0.2.8 calls.
//!
//! `kzg-rs` depends on `sp1_bls12_381`, which emits SP1 syscalls on
//! `target_os = "zkvm"`. OpenVM shares that target, so we wrap the software
//! curve types instead (`#[repr(transparent)]` so `kzg-rs`'s host `build.rs`
//! transmutes keep working). In the guest, G1 scalar multiplication and the
//! pairing check run on `OpenVM`'s `ecc` and `pairing` extensions; decoding and
//! scalar-field arithmetic stay in software.

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

/// Prepared G2 point for pairing. Keeps the affine point so the guest can hand
/// it to the `OpenVM` pairing intrinsic.
#[derive(Clone, Debug)]
pub struct G2Prepared {
    #[cfg(target_os = "zkvm")]
    affine: bls12_381::G2Affine,
    prepared: bls12_381::G2Prepared,
}

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

    /// Variable-base MSM, the SP1 helper `kzg-rs` calls. Runs on the `OpenVM`
    /// curve intrinsics in the guest.
    pub fn msm_variable_base(points: &[Self], scalars: &[Scalar]) -> Self {
        #[cfg(target_os = "zkvm")]
        {
            Self(intrinsics::g1_msm(points.iter().map(|p| p.0), scalars).into())
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            let mut acc = bls12_381::G1Projective::identity();
            for (point, scalar) in points.iter().zip(scalars) {
                acc += point.0 * scalar;
            }
            Self(acc)
        }
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
        Self {
            #[cfg(target_os = "zkvm")]
            affine: point.0,
            prepared: bls12_381::G2Prepared::from(point.0),
        }
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
        G1Projective::from(self) * rhs
    }
}

impl Mul<Scalar> for G1Projective {
    type Output = Self;

    fn mul(self, rhs: Scalar) -> Self {
        Self::msm_variable_base(&[self], &[rhs])
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
///
/// `kzg-rs` only compares `final_exponentiation()` of the result against the
/// identity. In the guest, a passing `OpenVM` pairing check returns the
/// identity loop result directly; a failing one falls back to software so the
/// result still exponentiates to the true non-identity value.
pub fn multi_miller_loop(terms: &[(&G1Affine, &G2Prepared)]) -> bls12_381::MillerLoopResult {
    #[cfg(target_os = "zkvm")]
    if intrinsics::pairing_check(terms) {
        return bls12_381::MillerLoopResult::default();
    }
    let mapped: alloc::vec::Vec<(&bls12_381::G1Affine, &bls12_381::G2Prepared)> =
        terms.iter().map(|(g1, g2)| (&g1.0, &g2.prepared)).collect();
    bls12_381::multi_miller_loop(&mapped)
}

#[cfg(target_os = "zkvm")]
mod intrinsics {
    //! Conversions into `OpenVM`'s BLS12-381 intrinsic types.
    //!
    //! Inputs are already-validated `bls12_381` points (subgroup-checked on
    //! decode, or derived from such points), which is what the intrinsic
    //! pairing check requires. Identities map to `(0, 0)`, which its Miller
    //! loop skips.

    use alloc::vec::Vec;

    use openvm_algebra_guest::{Field, IntMod};
    use openvm_ecc_guest::{AffinePoint, Group, msm, weierstrass::WeierstrassPoint};
    use openvm_pairing::{
        PairingCheck,
        bls12_381::{Bls12_381, Fp, Fp2, G1Affine as OvmG1, Scalar as OvmScalar},
    };

    use super::{G1Affine, G2Prepared, Scalar};

    const FP_BYTES: usize = 48;

    fn fp(be: &[u8]) -> Fp {
        Fp::from_be_bytes_unchecked(be)
    }

    /// `sum(scalars[i] * points[i])` on the `ecc` extension.
    pub(super) fn g1_msm(
        points: impl Iterator<Item = bls12_381::G1Projective>,
        scalars: &[Scalar],
    ) -> bls12_381::G1Affine {
        let bases: Vec<OvmG1> = points
            .map(|p| {
                let a = g1(&bls12_381::G1Affine::from(p));
                // SAFETY: `a` comes from a valid `bls12_381` point, so it is on the curve.
                unsafe { OvmG1::from_xy_unchecked(a.x, a.y) }
            })
            .collect();
        let coeffs: Vec<OvmScalar> =
            scalars.iter().map(|s| OvmScalar::from_le_bytes_unchecked(&s.to_bytes())).collect();
        let sum = msm(&coeffs, &bases);
        if sum.is_identity() {
            return bls12_381::G1Affine::identity();
        }
        let mut bytes = [0u8; 2 * FP_BYTES];
        bytes[..FP_BYTES].copy_from_slice(sum.x().to_be_bytes().as_ref());
        bytes[FP_BYTES..].copy_from_slice(sum.y().to_be_bytes().as_ref());
        // The intrinsic sum of subgroup points is a canonical subgroup point.
        bls12_381::G1Affine::from_uncompressed_unchecked(&bytes).unwrap()
    }

    fn g1(point: &bls12_381::G1Affine) -> AffinePoint<Fp> {
        if bool::from(point.is_identity()) {
            return AffinePoint::new(<Fp as Field>::ZERO, <Fp as Field>::ZERO);
        }
        let bytes = point.to_uncompressed();
        AffinePoint::new(fp(&bytes[..FP_BYTES]), fp(&bytes[FP_BYTES..]))
    }

    fn g2(point: &bls12_381::G2Affine) -> AffinePoint<Fp2> {
        if bool::from(point.is_identity()) {
            return AffinePoint::new(<Fp2 as Field>::ZERO, <Fp2 as Field>::ZERO);
        }
        // Uncompressed G2 is `x.c1 || x.c0 || y.c1 || y.c0`, big-endian.
        let bytes = point.to_uncompressed();
        let limb = |i: usize| fp(&bytes[i * FP_BYTES..(i + 1) * FP_BYTES]);
        AffinePoint::new(Fp2::new(limb(1), limb(0)), Fp2::new(limb(3), limb(2)))
    }

    /// Returns whether the product of pairings over `terms` is one.
    pub(super) fn pairing_check(terms: &[(&G1Affine, &G2Prepared)]) -> bool {
        let p: Vec<_> = terms.iter().map(|(a, _)| g1(&a.0)).collect();
        let q: Vec<_> = terms.iter().map(|(_, b)| g2(&b.affine)).collect();
        Bls12_381::pairing_check(&p, &q).is_ok()
    }
}
