//! `bls12_381` under the `sp1_bls12_381` crate name, plus the extra
//! `G1Projective::msm_variable_base` method `kzg-rs` 0.2.8 calls.
//!
//! `kzg-rs` depends on `sp1_bls12_381`, which emits SP1 syscalls on
//! `target_os = "zkvm"`. OpenVM shares that target, so we wrap the software
//! curve types instead (`#[repr(transparent)]` so `kzg-rs`'s host `build.rs`
//! transmutes keep working). In the guest, G1 decoding (with its subgroup
//! check), G1 scalar multiplication, and the pairing check run on `OpenVM`'s
//! `modular`, `ecc`, and `pairing` extensions, as do scalar multiplication,
//! inversion, and byte conversion; scalar addition stays in software.

#![no_std]

extern crate alloc;

use core::ops::{Add, AddAssign, Mul, MulAssign, Neg, Sub};

pub use bls12_381::Gt;
use subtle::{Choice, CtOption};

/// BLS12-381 scalar in `bls12_381`'s Montgomery-limb layout, which `kzg-rs`
/// relies on when it transmutes its precomputed roots of unity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Scalar(bls12_381::Scalar);

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

impl Scalar {
    /// Additive identity.
    pub const fn zero() -> Self {
        Self(bls12_381::Scalar::zero())
    }

    /// Multiplicative identity.
    pub const fn one() -> Self {
        Self(bls12_381::Scalar::one())
    }

    /// Converts little-endian 64-bit limbs, reducing them into the field.
    pub fn from_raw(limbs: [u64; 4]) -> Self {
        Self(bls12_381::Scalar::from_raw(limbs))
    }

    /// Decodes a canonical little-endian encoding.
    pub fn from_bytes(bytes: &[u8; 32]) -> CtOption<Self> {
        #[cfg(target_os = "zkvm")]
        {
            match intrinsics::scalar_from_bytes(bytes) {
                Some(scalar) => CtOption::new(scalar, Choice::from(1)),
                None => CtOption::new(Self::zero(), Choice::from(0)),
            }
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            bls12_381::Scalar::from_bytes(bytes).map(Self)
        }
    }

    /// Canonical little-endian encoding.
    pub fn to_bytes(&self) -> [u8; 32] {
        #[cfg(target_os = "zkvm")]
        {
            intrinsics::scalar_to_bytes(self)
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            self.0.to_bytes()
        }
    }

    /// Multiplicative inverse, or none for zero.
    pub fn invert(&self) -> CtOption<Self> {
        #[cfg(target_os = "zkvm")]
        {
            let is_some = *self != Self::zero();
            let inverse = if is_some { intrinsics::scalar_invert(self) } else { Self::zero() };
            CtOption::new(inverse, Choice::from(u8::from(is_some)))
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            self.0.invert().map(Self)
        }
    }

    /// `self^by`, with `by` as little-endian 64-bit limbs. Variable time; the
    /// exponents `kzg-rs` uses are public.
    pub fn pow(&self, by: &[u64; 4]) -> Self {
        let mut acc = Self::one();
        for limb in by.iter().rev() {
            for bit in (0..64).rev() {
                acc *= acc;
                if (limb >> bit) & 1 == 1 {
                    acc *= *self;
                }
            }
        }
        acc
    }
}

impl From<u64> for Scalar {
    fn from(value: u64) -> Self {
        Self(bls12_381::Scalar::from(value))
    }
}

impl Mul for Scalar {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self {
        #[cfg(target_os = "zkvm")]
        {
            intrinsics::scalar_mul(&self, &rhs)
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            Self(self.0 * rhs.0)
        }
    }
}

impl Mul<&Scalar> for Scalar {
    type Output = Self;

    fn mul(self, rhs: &Self) -> Self {
        self * *rhs
    }
}

impl Mul<Scalar> for &Scalar {
    type Output = Scalar;

    fn mul(self, rhs: Scalar) -> Scalar {
        *self * rhs
    }
}

impl MulAssign for Scalar {
    fn mul_assign(&mut self, rhs: Self) {
        *self = *self * rhs;
    }
}

impl Add for Scalar {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for Scalar {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for Scalar {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
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

    /// Decode from compressed bytes, rejecting points outside the prime-order
    /// subgroup.
    pub fn from_compressed(bytes: &[u8; 48]) -> CtOption<Self> {
        #[cfg(target_os = "zkvm")]
        {
            let point = intrinsics::g1_from_compressed(bytes);
            CtOption::new(Self(point.unwrap_or_default()), Choice::from(u8::from(point.is_some())))
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            bls12_381::G1Affine::from_compressed(bytes).map(Self)
        }
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
            let projective: alloc::vec::Vec<_> = points.iter().map(|p| p.0).collect();
            let mut affine = alloc::vec![bls12_381::G1Affine::identity(); points.len()];
            bls12_381::G1Projective::batch_normalize(&projective, &mut affine);
            Self(intrinsics::g1_msm(&affine, scalars).into())
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            let mut acc = bls12_381::G1Projective::identity();
            for (point, scalar) in points.iter().zip(scalars) {
                acc += point.0 * scalar.0;
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
        #[cfg(target_os = "zkvm")]
        {
            G1Projective(intrinsics::g1_msm(&[self.0], &[rhs]).into())
        }
        #[cfg(not(target_os = "zkvm"))]
        {
            G1Projective::from(self) * rhs
        }
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
        G2Projective(self.0 * rhs.0)
    }
}

impl Mul<Scalar> for G2Projective {
    type Output = Self;

    fn mul(self, rhs: Scalar) -> Self {
        Self(self.0 * rhs.0)
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
        self.0.eq(&other.0)
    }
}

impl Eq for G1Affine {}

impl PartialEq for G2Affine {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
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
    use core::mem::transmute;

    use openvm_algebra_guest::{DivUnsafe, Field, IntMod, Sqrt};
    use openvm_ecc_guest::{AffinePoint, Group, msm, weierstrass::WeierstrassPoint};
    use openvm_pairing::{
        PairingCheck,
        bls12_381::{Bls12_381, Fp, Fp2, G1Affine as OvmG1, Scalar as OvmScalar},
    };

    use super::{G1Affine, G2Prepared, Scalar};

    const FP_BYTES: usize = 48;

    /// `R^-1 mod r` for the Montgomery radix `R = 2^256`, little-endian.
    const R_INV: [u8; 32] = [
        0x40, 0xc0, 0x75, 0xfe, 0x69, 0x5b, 0xf7, 0x13, 0x5f, 0x70, 0xdc, 0x09, 0x8f, 0xca, 0x6f,
        0xab, 0x6a, 0x26, 0x77, 0x4f, 0x8a, 0x07, 0x04, 0x72, 0x57, 0x9d, 0x00, 0x30, 0x93, 0x86,
        0xbe, 0x1b,
    ];

    /// The cube root of unity in `Fp` that `bls12_381` uses for the G1
    /// endomorphism `(x, y) -> (BETA * x, y)`, big-endian.
    const BETA: [u8; FP_BYTES] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5f, 0x19, 0x67, 0x2f, 0xdf, 0x76, 0xce,
        0x51, 0xba, 0x69, 0xc6, 0x07, 0x6a, 0x0f, 0x77, 0xea, 0xdd, 0xb3, 0xa9, 0x3b, 0xe6, 0xf8,
        0x96, 0x88, 0xde, 0x17, 0xd8, 0x13, 0x62, 0x0a, 0x00, 0x02, 0x2e, 0x01, 0xff, 0xff, 0xff,
        0xfe, 0xff, 0xfe,
    ];

    /// `|x|` for the BLS12-381 parameter `x = -0xd201000000010000`.
    const BLS_X_ABS: u64 = 0xd201_0000_0001_0000;

    /// The Montgomery form `a * R mod r` as an intrinsic integer. `bls12_381`
    /// stores it as four reduced little-endian `u64` limbs, i.e. 32 LE bytes.
    fn montgomery(scalar: &Scalar) -> OvmScalar {
        // SAFETY: `bls12_381::Scalar` is `[u64; 4]`, the layout `kzg-rs` already
        // transmutes, and every bit pattern of 32 bytes is a valid array.
        let bytes: [u8; 32] = unsafe { transmute(scalar.0) };
        OvmScalar::from_le_bytes_unchecked(&bytes)
    }

    /// Inverse of [`montgomery`]. Asserts the intrinsic result is canonical so
    /// the software limb arithmetic stays sound.
    fn from_montgomery(value: OvmScalar) -> Scalar {
        value.assert_reduced();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(value.as_le_bytes());
        // SAFETY: as in `montgomery`; the value is reduced, as `bls12_381` requires.
        Scalar(unsafe { transmute::<[u8; 32], bls12_381::Scalar>(bytes) })
    }

    /// `(aR)(bR)R^-1 = abR`.
    pub(super) fn scalar_mul(a: &Scalar, b: &Scalar) -> Scalar {
        from_montgomery(montgomery(a) * montgomery(b) * OvmScalar::from_le_bytes_unchecked(&R_INV))
    }

    /// `R^2 / (aR) = a^-1 R`. `scalar` must be non-zero.
    pub(super) fn scalar_invert(scalar: &Scalar) -> Scalar {
        let r = montgomery(&Scalar::one());
        from_montgomery((r.clone() * &r).div_unsafe(montgomery(scalar)))
    }

    /// Canonical `a` to `aR`; `None` if `bytes` is not below the modulus.
    pub(super) fn scalar_from_bytes(bytes: &[u8; 32]) -> Option<Scalar> {
        let value = OvmScalar::from_le_bytes(bytes)?;
        Some(from_montgomery(value * montgomery(&Scalar::one())))
    }

    /// `aR * R^-1 = a`, little-endian.
    pub(super) fn scalar_to_bytes(scalar: &Scalar) -> [u8; 32] {
        let value = montgomery(scalar) * OvmScalar::from_le_bytes_unchecked(&R_INV);
        value.assert_reduced();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(value.as_le_bytes());
        bytes
    }

    fn fp(be: &[u8]) -> Fp {
        Fp::from_be_bytes_unchecked(be)
    }

    /// `sum(scalars[i] * points[i])` on the `ecc` extension.
    pub(super) fn g1_msm(
        points: &[bls12_381::G1Affine],
        scalars: &[Scalar],
    ) -> bls12_381::G1Affine {
        let bases: Vec<OvmG1> = points
            .iter()
            .map(|p| {
                let a = g1(p);
                // SAFETY: `a` comes from a valid `bls12_381` point, so it is on the curve.
                unsafe { OvmG1::from_xy_unchecked(a.x, a.y) }
            })
            .collect();
        let coeffs: Vec<OvmScalar> =
            scalars.iter().map(|s| OvmScalar::from_le_bytes_unchecked(&s.to_bytes())).collect();
        // The intrinsic sum of subgroup points is a subgroup point.
        to_bls(&msm(&coeffs, &bases))
    }

    /// `bls12_381::G1Affine::from_compressed`: the same flag rules and subgroup
    /// check, with the square root and curve arithmetic on intrinsics.
    pub(super) fn g1_from_compressed(bytes: &[u8; FP_BYTES]) -> Option<bls12_381::G1Affine> {
        let compressed = (bytes[0] >> 7) & 1 == 1;
        let infinity = (bytes[0] >> 6) & 1 == 1;
        let sort = (bytes[0] >> 5) & 1 == 1;
        let mut x_bytes = *bytes;
        x_bytes[0] &= 0b0001_1111;
        let x = Fp::from_be_bytes(&x_bytes)?;
        if !compressed {
            return None;
        }
        if infinity {
            return (!sort && x == <Fp as Field>::ZERO).then(bls12_381::G1Affine::identity);
        }

        let y = (&x * &x * &x + Fp::from_u8(4)).sqrt()?;
        let neg_y = -&y;
        neg_y.assert_reduced();
        let largest = y.as_le_bytes().iter().rev().cmp(neg_y.as_le_bytes().iter().rev()).is_gt();
        let y = if largest == sort { y } else { neg_y };
        // SAFETY: `sqrt` verified `y^2 = x^3 + 4`.
        let point = unsafe { OvmG1::from_xy_unchecked(x, y) };

        // P is in G1 iff endo(P) == -[x^2]P (eprint 2022/352), as `bls12_381` checks.
        // SAFETY: the endomorphism maps curve points to curve points.
        let endo = unsafe {
            OvmG1::from_xy_unchecked(
                point.x() * &Fp::from_be_bytes_unchecked(&BETA),
                point.y().clone(),
            )
        };
        if endo != -mul_by_x_abs(&mul_by_x_abs(&point)) {
            return None;
        }
        Some(to_bls(&point))
    }

    /// `[|x|]P` for the BLS parameter `x`, by double-and-add on the complete
    /// group law.
    fn mul_by_x_abs(point: &OvmG1) -> OvmG1 {
        let mut acc = <OvmG1 as Group>::IDENTITY;
        let mut tmp = point.clone();
        // Bit 0 of `BLS_X_ABS` is clear.
        let mut bits = BLS_X_ABS >> 1;
        while bits != 0 {
            tmp.double_assign();
            if bits & 1 == 1 {
                acc = &acc + &tmp;
            }
            bits >>= 1;
        }
        acc
    }

    /// Converts a subgroup point on the intrinsic curve to `bls12_381`.
    fn to_bls(point: &OvmG1) -> bls12_381::G1Affine {
        if point.is_identity() {
            return bls12_381::G1Affine::identity();
        }
        let mut bytes = [0u8; 2 * FP_BYTES];
        bytes[..FP_BYTES].copy_from_slice(point.x().to_be_bytes().as_ref());
        bytes[FP_BYTES..].copy_from_slice(point.y().to_be_bytes().as_ref());
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

#[cfg(test)]
mod tests {
    use super::Scalar;

    #[test]
    fn pow_matches_bls12_381() {
        let base = Scalar::from(0x1234_5678_9abc_def0);
        for exp in [[0, 0, 0, 0], [1, 0, 0, 0], [4096, 0, 0, 0], [u64::MAX, 7, 0, 1]] {
            assert_eq!(base.pow(&exp).0, base.0.pow(&exp));
        }
    }
}
