//! Small division using reciprocals from [MG10].
//!
//! [MG10]: https://gmplib.org/~tege/division-paper.pdf

// Following naming from paper.
#![allow(clippy::many_single_char_names, clippy::similar_names)]
// Truncation is intentional
#![allow(clippy::cast_possible_truncation)]

// The running time is 2.7 ns for [`div_2x1_mg10`] versus 18 ns for
// [`div_2x1_ref`].
pub use self::{div_2x1_mg10 as div_2x1, div_3x2_mg10 as div_3x2};
use super::reciprocal::{reciprocal, reciprocal_2};
use crate::{
    algorithms::DW,
    utils::{UncheckedSlice, unlikely},
};

/// ⚠️ Compute single limb division.
#[doc = crate::algorithms::unstable_warning!()]
#[inline(always)]
#[track_caller]
#[must_use]
pub const fn div_1x1(numerator: u64, divisor: u64) -> (u64, u64) {
    (numerator / divisor, numerator % divisor)
}

/// ⚠️ Compute single limb normalized division.
#[doc = crate::algorithms::unstable_warning!()]
/// The divisor must be normalized. See algorithm 7 from [MG10].
///
/// # Safety
///
/// - The input `d` must be at least `2^63`, i.e., its highest bit must be set.
///
/// [MG10]: https://gmplib.org/~tege/division-paper.pdf
#[inline(always)]
pub unsafe fn div_nx1_normalized(u: &mut [u64], d: u64) -> u64 {
    // OPT: Version with in-place shifting of `u`
    debug_assert!(d >= (1 << 63));

    // SAFETY: COU propagated to this function.
    let v = unsafe { reciprocal(d) };
    let mut r: u64 = 0;
    for u in u.iter_mut().rev() {
        let n = DW::join(r, *u);
        let (q, r0) = div_2x1(n, d, v);
        *u = q;
        r = r0;
    }
    r
}

/// ⚠️ Compute single limb division.
#[doc = crate::algorithms::unstable_warning!()]
/// See algorithm 7 from [MG10].
///
/// [MG10]: https://gmplib.org/~tege/division-paper.pdf
///
/// # Safety
///
/// The following conditions must hold:
///
/// - The highest limb of `numerator` must be nonzero.
/// - The highest limb of `divisor` must be nonzero.
///
/// In debug, may panic if any condition of use is violated. In release,
/// may panic or trigger UB if any condition of use is violated.
#[inline(always)]
pub unsafe fn div_nx1(limbs: &mut [u64], divisor: u64) -> u64 {
    debug_assert!(divisor != 0);
    debug_assert!(!limbs.is_empty());
    debug_assert!(*limbs.last().unwrap() != 0);

    let limbs = UncheckedSlice::wrap_mut(limbs);

    // Normalize and compute reciprocal
    let shift = divisor.leading_zeros();
    if shift == 0 {
        return unsafe { div_nx1_normalized(limbs, divisor) };
    }
    let divisor = divisor << shift;
    let reciprocal = unsafe { reciprocal(divisor) };

    let last = limbs[limbs.len() - 1];
    let mut remainder = last >> (64 - shift);
    for i in (1..limbs.len()).rev() {
        // Shift limbs
        let upper = limbs[i];
        let lower = limbs[i - 1];
        let u = (upper << shift) | (lower >> (64 - shift));

        // Compute quotient
        let n = DW::join(remainder, u);
        let (q, r) = div_2x1(n, divisor, reciprocal);

        // Store quotient
        limbs[i] = q;
        remainder = r;
    }
    // Compute last quotient
    let first = limbs[0];
    let n = DW::join(remainder, first << shift);
    let (q, remainder) = div_2x1(n, divisor, reciprocal);
    limbs[0] = q;

    // Un-normalize remainder
    remainder >> shift
}

/// ⚠️ Compute double limb normalized division.
#[doc = crate::algorithms::unstable_warning!()]
/// Same as [`div_nx1`] but using [`div_3x2`] internally.
///
/// # Safety
///
/// - The divisor must be normalized (highest bit is set).
///
/// In debug, may panic if any condition of use is violated. In release,
/// may panic or trigger UB if any condition of use is violated.
#[inline(always)]
pub unsafe fn div_nx2_normalized(u: &mut [u64], d: u128) -> u128 {
    // OPT: Version with in-place shifting of `u`
    debug_assert!(d >= (1 << 127));

    // SAFETY: d >= 2**127 - propagated to this function.
    let v = unsafe { reciprocal_2(d) };
    let mut remainder: u128 = 0;
    for u in u.iter_mut().rev() {
        // SAFETY:
        // div_3x2 COUs:
        // - d >= 2**127 - propagated to this function.
        // - remainder < d - initially 0, then follows from div_3x2 correctness.
        // - v = reciprocal_2(d) - calculated above
        let (q, r) = unsafe { div_3x2(remainder, *u, d, v) };
        *u = q;
        remainder = r;
    }
    remainder
}

/// ⚠️ Compute double limb division.
#[doc = crate::algorithms::unstable_warning!()]
/// Same as [`div_nx2_normalized`] but does the shifting of the numerator
/// inline.
///
/// # Safety
///
/// - The highest limb of `numerator` must be nonzero.
/// - The divisor's high word is non-zero (i.e., divisor >= $2^{64}$).
///
/// In debug, may panic if any condition of use is violated. In release,
/// may panic or trigger UB if any condition of use is violated.
#[inline(always)]
pub unsafe fn div_nx2(limbs: &mut [u64], divisor: u128) -> u128 {
    debug_assert!(divisor >= 1 << 64);
    debug_assert!(!limbs.is_empty());
    debug_assert!(*limbs.last().unwrap() != 0);

    let limbs = UncheckedSlice::wrap_mut(limbs);

    // Normalize and compute reciprocal
    let shift = DW::high(divisor).leading_zeros();
    if shift == 0 {
        return unsafe { div_nx2_normalized(limbs, divisor) };
    }
    let divisor = divisor << shift;
    let reciprocal = unsafe { reciprocal_2(divisor) };

    let last = limbs[limbs.len() - 1];
    let mut remainder: u128 = u128::from(last >> (64 - shift));
    for i in (1..limbs.len()).rev() {
        // Shift limbs
        let upper = limbs[i];
        let lower = limbs[i - 1];
        let u = (upper << shift) | (lower >> (64 - shift));

        // Compute quotient
        let (q, r) = unsafe { div_3x2(remainder, u, divisor, reciprocal) };

        // Store quotient
        limbs[i] = q;
        remainder = r;
    }
    // Compute last quotient
    let first = limbs[0];
    let (q, remainder) = unsafe { div_3x2(remainder, first << shift, divisor, reciprocal) };
    limbs[0] = q;

    // Un-normalize remainder
    remainder >> shift
}

/// ⚠️ Reference implementation for `div_2x1`.
#[doc = crate::algorithms::unstable_warning!()]
#[inline]
#[must_use]
pub fn div_2x1_ref(u: u128, d: u64) -> (u64, u64) {
    debug_assert!(d >= (1 << 63));
    debug_assert!((u >> 64) < u128::from(d));
    let d = u128::from(d);
    let q = (u / d) as u64;
    let r = (u % d) as u64;
    (q, r)
}

/// ⚠️ Computes the quotient and remainder of a `u128` divided by a `u64`.
#[doc = crate::algorithms::unstable_warning!()]
/// Implements algorithm 4 from [MG10].
///
/// # Safety
///
/// - `u < d * 2**64`,
/// - `d >= 2**63`, and
/// - `v = reciprocal(d)`.
///
/// In debug, panics if any condition of use is violated. In release,
/// may panic or trigger UB if any condition of use is violated.
///
/// [MG10]: https://gmplib.org/~tege/division-paper.pdf
#[inline(always)]
#[must_use]
pub fn div_2x1_mg10(u: u128, d: u64, v: u64) -> (u64, u64) {
    debug_assert!(d >= (1 << 63));
    debug_assert!((u >> 64) < u128::from(d));
    debug_assert_eq!(v, unsafe { reciprocal(d) });

    let q = u + (u >> 64) * u128::from(v);
    let q0 = q as u64;
    let q1 = ((q >> 64) as u64).wrapping_add(1);
    let r = (u as u64).wrapping_sub(q1.wrapping_mul(d));
    let (q1, r) = if r > q0 { (q1.wrapping_sub(1), r.wrapping_add(d)) } else { (q1, r) };
    let (q1, r) = if unlikely(r >= d) { (q1.wrapping_add(1), r.wrapping_sub(d)) } else { (q1, r) };
    (q1, r)
}

/// ⚠️ Reference implementation for `div_3x2`.
#[doc = crate::algorithms::unstable_warning!()]
/// TODO: This implementation is off by one.
#[inline]
#[must_use]
pub fn div_3x2_ref(n21: u128, n0: u64, d: u128) -> u64 {
    debug_assert!(d >= (1 << 127));
    debug_assert!(n21 < d);

    let n2 = (n21 >> 64) as u64;
    let n1 = n21 as u64;
    let d1 = (d >> 64) as u64;
    let d0 = d as u64;

    if unlikely(n2 == d1) {
        // From [n2 n1] < [d1 d0] and n2 = d1 it follows that n[1] < d[0].
        debug_assert!(n1 < d0);
        // We start by subtracting 2^64 times the divisor, resulting in a
        // negative remainder. Depending on the result, we need to add back
        // in one or two times the divisor to make the remainder positive.
        // (It can not be more since the divisor is > 2^127 and the negated
        // remainder is < 2^128.)
        let neg_remainder = u128::from(d0).wrapping_sub((u128::from(n1) << 64) | u128::from(n0));
        if neg_remainder > d { 0xffff_ffff_ffff_fffe_u64 } else { 0xffff_ffff_ffff_ffff_u64 }
    } else {
        // Compute quotient and remainder
        let (mut q, mut r) = div_2x1_ref(n21, d1);

        let t1 = u128::from(q) * u128::from(d0);
        let t2 = (u128::from(n0) << 64) | u128::from(r);
        if t1 > t2 {
            q -= 1;
            r = r.wrapping_add(d1);
            let overflow = r < d1;
            if !overflow {
                let t1 = u128::from(q) * u128::from(d0);
                let t2 = (u128::from(n0) << 64) | u128::from(r);
                if t1 > t2 {
                    q -= 1;
                    // UNUSED: r += d[1];
                }
            }
        }
        q
    }
}

/// ⚠️ Computes the quotient of a 192 bits divided by a normalized u128.
#[doc = crate::algorithms::unstable_warning!()]
/// Implements [MG10] algorithm 5.
///
/// # Safety
///
/// - d >= 2**127,
/// - u21 < d
/// - v = reciprocal_2(d)
///
/// In debug, panics if any condition of use is violated. In release,
/// may panic or trigger UB if any condition of use is violated.
///
/// [MG10]: https://gmplib.org/~tege/division-paper.pdf
#[inline(always)]
#[must_use]
pub unsafe fn div_3x2_mg10(u21: u128, u0: u64, d: u128, v: u64) -> (u64, u128) {
    debug_assert!(d >= (1 << 127));
    debug_assert!(u21 < d);
    debug_assert_eq!(v, unsafe { reciprocal_2(d) });

    let q = DW::mul(DW::high(u21), v) + u21;
    let r1 = DW::low(u21).wrapping_sub(DW::high(q).wrapping_mul(DW::high(d)));
    let t = DW::mul(DW::low(d), DW::high(q));
    let mut r = DW::join(r1, u0).wrapping_sub(t).wrapping_sub(d);
    let mut q1 = DW::high(q).wrapping_add(1);
    if DW::high(r) >= DW::low(q) {
        q1 = q1.wrapping_sub(1);
        r = r.wrapping_add(d);
    }
    if unlikely(r >= d) {
        q1 = q1.wrapping_add(1);
        r = r.wrapping_sub(d);
    }
    (q1, r)
}

#[cfg(test)]
mod tests {
    use proptest::{
        collection,
        num::{u64, u128},
        prop_assume, proptest,
        strategy::Strategy,
    };

    use super::*;
    use crate::algorithms::addmul;

    #[test]
    fn test_div_2x1_mg10() {
        proptest!(|(q: u64, r: u64, mut d: u64)| {
            let d = d | (1 << 63);
            let r = r % d;
            let n = u128::from(q) * u128::from(d) + u128::from(r);
            let v = unsafe { reciprocal(d) };
            assert_eq!(div_2x1_mg10(n, d, v), (q,r));
        });
    }

    #[ignore = "TODO"]
    #[test]
    fn test_div_3x2_ref() {
        proptest!(|(q: u64, r: u128, mut d: u128)| {
            let d = d | (1 << 127);
            let r = r % d;
            let (n21, n0) = {
                let d1 = (d >> 64) as u64;
                let d0 = d as u64;
                let r1 = (r >> 64) as u64;
                let r0 = r as u64;
                // n = q * d + r
                let n10 = u128::from(q) * u128::from(d0) + u128::from(r0);
                let n0 = n10 as u64;
                let n21 = (n10 >> 64) + u128::from(q) * u128::from(d1) + u128::from(r1);
                (n21, n0)
            };
            assert_eq!(div_3x2_ref(n21, n0, d), q);
        });
    }

    #[test]
    fn test_div_3x2_mg10() {
        proptest!(|(q: u64, r: u128, mut d: u128)| {
            let d = d | (1 << 127);
            let r = r % d;
            let (n21, n0) = {
                let d1 = (d >> 64) as u64;
                let d0 = d as u64;
                let r1 = (r >> 64) as u64;
                let r0 = r as u64;
                // n = q * d + r
                let n10 = u128::from(q) * u128::from(d0) + u128::from(r0);
                let n0 = n10 as u64;
                let n21 = (n10 >> 64) + u128::from(q) * u128::from(d1) + u128::from(r1);
                (n21, n0)
            };
            let v = unsafe { reciprocal_2(d) };
            assert_eq!(unsafe { div_3x2_mg10(n21, n0, d, v) }, (q, r));
        });
    }

    #[test]
    fn test_div_nx1_normalized() {
        let any_vec = collection::vec(u64::ANY, ..10);
        proptest!(|(quotient in any_vec, mut divisor: u64, mut remainder: u64)| {
            // Construct problem
            divisor |= 1 << 63;
            remainder %= divisor;
            let mut numerator = vec![0; quotient.len() + 1];
            numerator[0] = remainder;
            addmul(&mut numerator, &quotient, &[divisor]);

            // Test
            let r = unsafe { div_nx1_normalized(&mut numerator, divisor) };
            assert_eq!(&numerator[..quotient.len()], &quotient);
            assert_eq!(r, remainder);
        });
    }

    #[test]
    fn test_div_nx1() {
        let any_vec = collection::vec(u64::ANY, 1..10);
        let divrem = (1..u64::MAX, u64::ANY).prop_map(|(d, r)| (d, r % d));
        proptest!(|(quotient in any_vec,(divisor, remainder) in divrem)| {
            // Construct problem
            let mut numerator = vec![0; quotient.len() + 1];
            numerator[0] = remainder;
            addmul( &mut numerator, &quotient, &[divisor]);

            // Trim numerator
            while numerator.last() == Some(&0) {
                numerator.pop();
            }
            prop_assume!(!numerator.is_empty());

            // Test
            let r = unsafe { div_nx1(&mut numerator, divisor) };
            assert_eq!(&numerator[..quotient.len()], &quotient);
            assert_eq!(r, remainder);
        });
    }

    #[test]
    fn test_div_nx2_normalized() {
        let any_vec = collection::vec(u64::ANY, ..10);
        let divrem = (1_u128 << 127.., u128::ANY).prop_map(|(d, r)| (d, r % d));
        proptest!(|(quotient in any_vec, (divisor, remainder) in divrem)| {
            // Construct problem
            let mut numerator = vec![0; quotient.len() + 2];
            numerator[0] = DW::low(remainder);
            numerator[1] = DW::high(remainder);
            addmul(&mut numerator, &quotient, &[DW::low(divisor), DW::high(divisor)]);

            // Test
            let r = unsafe { div_nx2_normalized(&mut numerator, divisor) };
            assert_eq!(&numerator[..quotient.len()], &quotient);
            assert_eq!(r, remainder);
        });
    }

    #[test]
    fn test_div_nx2() {
        let any_vec = collection::vec(u64::ANY, 2..10);
        let divrem = (1..u128::MAX, u128::ANY).prop_map(|(d, r)| (d, r % d));
        proptest!(|(quotient in any_vec,(divisor, remainder) in divrem)| {
            // Construct problem
            let mut numerator = vec![0; quotient.len() + 2];
            numerator[0] = DW::low(remainder);
            numerator[1] = DW::high(remainder);
            addmul(&mut numerator, &quotient, &[DW::low(divisor), DW::high(divisor)]);

            // Trim numerator
            while numerator.last() == Some(&0) {
                numerator.pop();
            }
            prop_assume!(!numerator.is_empty());

            // Test
            let r = unsafe { div_nx2(&mut numerator, divisor) };
            assert_eq!(&numerator[..quotient.len()], &quotient);
            assert_eq!(r, remainder);
        });
    }
}
