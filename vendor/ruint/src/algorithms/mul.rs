#![allow(clippy::module_name_repetitions)]

use crate::algorithms::{DW, borrowing_sub};

/// ⚠️ Computes `result += a * b` and checks for overflow.
#[doc = crate::algorithms::unstable_warning!()]
/// Arrays are in little-endian order. All arrays can be arbitrary sized.
///
/// # Algorithm
///
/// Trims zeros from inputs, then uses the schoolbook multiplication algorithm.
/// It takes the shortest input as the outer loop.
///
/// # Examples
///
/// ```
/// # use ruint::algorithms::addmul;
/// let mut result = [0];
/// let overflow = addmul(&mut result, &[3], &[4]);
/// assert_eq!(overflow, false);
/// assert_eq!(result, [12]);
/// ```
#[inline(always)]
pub const fn addmul(mut lhs: &mut [u64], mut a: &[u64], mut b: &[u64]) -> bool {
    // Trim zeros from `a`
    while let [0, rest @ ..] = a {
        a = rest;
        if let [_, rest @ ..] = lhs {
            lhs = rest;
        }
    }
    a = super::trim_end_zeros(a);
    if a.is_empty() {
        return false;
    }

    // Trim zeros from `b`
    while let [0, rest @ ..] = b {
        b = rest;
        if let [_, rest @ ..] = lhs {
            lhs = rest;
        }
    }
    b = super::trim_end_zeros(b);
    if b.is_empty() {
        return false;
    }

    if lhs.is_empty() {
        return true;
    }

    let (a, b) = if b.len() > a.len() { (b, a) } else { (a, b) };

    // Iterate over limbs of `b` and add partial products to `lhs`.
    let mut overflow = false;
    const_range_for!(&b in ref b => {
        if lhs.len() >= a.len() {
            let (target, rest) = lhs.split_at_mut(a.len());
            let carry = addmul_nx1(target, a, b);
            let carry = add_nx1(rest, carry);
            overflow |= carry != 0;
        } else {
            overflow = true;
            if lhs.is_empty() {
                break;
            }
            let (a, _) = a.split_at(lhs.len());
            addmul_nx1(lhs, a, b);
        }
        (_, lhs) = lhs.split_at_mut(1);
    });
    overflow
}

const ADDMUL_N_SMALL_LIMIT: usize = 8;

/// ⚠️ Computes wrapping `result += a * b`, with a fast-path for when all inputs
/// are the same length and small enough.
#[doc = crate::algorithms::unstable_warning!()]
/// See [`addmul`] for more details.
#[inline(always)]
pub const fn addmul_n(lhs: &mut [u64], a: &[u64], b: &[u64]) {
    let n = lhs.len();
    if n <= ADDMUL_N_SMALL_LIMIT && a.len() == n && b.len() == n {
        addmul_n_small(lhs, a, b);
    } else {
        let _ = addmul(lhs, a, b);
    }
}

#[inline(always)]
const fn addmul_n_small(lhs: &mut [u64], a: &[u64], b: &[u64]) {
    let n = lhs.len();
    assume!(n <= ADDMUL_N_SMALL_LIMIT);
    assume!(a.len() == n);
    assume!(b.len() == n);

    const_range_for!(j in 0..n => {
        let mut carry = 0;
        // Widening multiply-accumulate for all but the last position.
        let end = n - j - 1;
        const_range_for!(i in 0..end => {
            (lhs[j + i], carry) = DW::split(DW::muladd2(a[i], b[j], carry, lhs[j + i]));
        });
        // Last position: the carry out is discarded (it would go beyond n
        // limbs), so use truncated arithmetic to reduce register pressure.
        lhs[j + end] = (a[end].wrapping_mul(b[j]))
            .wrapping_add(lhs[j + end])
            .wrapping_add(carry);
    });
}

/// ⚠️ Computes `lhs += a` and returns the carry.
#[doc = crate::algorithms::unstable_warning!()]
#[inline(always)]
pub const fn add_nx1(lhs: &mut [u64], mut a: u64) -> u64 {
    if a == 0 {
        return 0;
    }
    const_range_for!(lhs in mut *lhs => {
        (*lhs, a) = DW::split(DW::add(*lhs, a));
        if a == 0 {
            return 0;
        }
    });
    a
}

/// ⚠️ Computes `lhs *= a` and returns the carry.
#[doc = crate::algorithms::unstable_warning!()]
#[inline(always)]
pub const fn mul_nx1(lhs: &mut [u64], a: u64) -> u64 {
    let mut carry = 0;
    const_range_for!(lhs in mut *lhs => {
        (*lhs, carry) = DW::split(DW::muladd(*lhs, a, carry));
    });
    carry
}

/// ⚠️ Computes `lhs += a * b` and returns the carry.
#[doc = crate::algorithms::unstable_warning!()]
/// Requires `lhs.len() == a.len()`.
///
/// $$
/// \begin{aligned}
/// \mathsf{lhs'} &= \mod{\mathsf{lhs} + \mathsf{a} ⋅ \mathsf{b}}_{2^{64⋅N}}
/// \\\\ \mathsf{carry} &= \floor{\frac{\mathsf{lhs} + \mathsf{a} ⋅ \mathsf{b}
/// }{2^{64⋅N}}} \end{aligned}
/// $$
#[inline(always)]
pub const fn addmul_nx1(lhs: &mut [u64], a: &[u64], b: u64) -> u64 {
    assume!(lhs.len() == a.len());
    let mut carry = 0;
    const_range_for!(i in 0..a.len() => {
        (lhs[i], carry) = DW::split(DW::muladd2(a[i], b, carry, lhs[i]));
    });
    carry
}

/// ⚠️ Computes `lhs -= a * b` and returns the borrow.
#[doc = crate::algorithms::unstable_warning!()]
/// Requires `lhs.len() == a.len()`.
///
/// $$
/// \begin{aligned}
/// \mathsf{lhs'} &= \mod{\mathsf{lhs} - \mathsf{a} ⋅ \mathsf{b}}_{2^{64⋅N}}
/// \\\\ \mathsf{borrow} &= \floor{\frac{\mathsf{a} ⋅ \mathsf{b} -
/// \mathsf{lhs}}{2^{64⋅N}}} \end{aligned}
/// $$
// OPT: `carry` and `borrow` can probably be merged into a single var.
#[inline(always)]
pub const fn submul_nx1(lhs: &mut [u64], a: &[u64], b: u64) -> u64 {
    assume!(lhs.len() == a.len());
    let mut carry = 0;
    let mut borrow = false;
    const_range_for!(i in 0..a.len() => {
        // Compute product limbs
        let limb;
        (limb, carry) = DW::split(DW::muladd(a[i], b, carry));

        // Subtract
        (lhs[i], borrow) = borrowing_sub(lhs[i], limb, borrow);
    });
    borrow as u64 + carry
}

#[cfg(test)]
mod tests {
    use proptest::{collection, num::u64, proptest};

    use super::*;

    #[allow(clippy::cast_possible_truncation)] // Intentional truncation.
    fn addmul_ref(result: &mut [u64], a: &[u64], b: &[u64]) -> bool {
        let mut overflow = 0;
        for (i, a) in a.iter().copied().enumerate() {
            let mut result = result.iter_mut().skip(i);
            let mut b = b.iter().copied();
            let mut carry = 0_u128;
            loop {
                match (result.next(), b.next()) {
                    // Partial product.
                    (Some(result), Some(b)) => {
                        carry += u128::from(*result) + u128::from(a) * u128::from(b);
                        *result = carry as u64;
                        carry >>= 64;
                    }
                    // Carry propagation.
                    (Some(result), None) => {
                        carry += u128::from(*result);
                        *result = carry as u64;
                        carry >>= 64;
                    }
                    // Excess product.
                    (None, Some(b)) => {
                        carry += u128::from(a) * u128::from(b);
                        overflow |= carry as u64;
                        carry >>= 64;
                    }
                    // Fin.
                    (None, None) => {
                        break;
                    }
                }
            }
            overflow |= carry as u64;
        }
        overflow != 0
    }

    #[test]
    fn test_addmul() {
        let any_vec = collection::vec(u64::ANY, 0..10);
        proptest!(|(mut lhs in &any_vec, a in &any_vec, b in &any_vec)| {
            // Reference
            let mut ref_lhs = lhs.clone();
            let ref_overflow = addmul_ref(&mut ref_lhs, &a, &b);

            // Test
            let overflow = addmul(&mut lhs, &a, &b);
            assert_eq!(lhs, ref_lhs);
            assert_eq!(overflow, ref_overflow);
        });
    }

    fn test_vals(lhs: &[u64], rhs: &[u64], expected: &[u64], expected_overflow: bool) {
        let mut result = vec![0; expected.len()];
        let overflow = addmul(&mut result, lhs, rhs);
        assert_eq!(overflow, expected_overflow);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_empty() {
        test_vals(&[], &[], &[], false);
        test_vals(&[], &[1], &[], false);
        test_vals(&[1], &[], &[], false);
        test_vals(&[1], &[1], &[], true);
        test_vals(&[], &[], &[0], false);
        test_vals(&[], &[1], &[0], false);
        test_vals(&[1], &[], &[0], false);
        test_vals(&[1], &[1], &[1], false);
    }

    #[test]
    fn test_submul_nx1() {
        let mut lhs = [
            15520854688669198950,
            13760048731709406392,
            14363314282014368551,
            13263184899940581802,
        ];
        let a =
            [7955980792890017645, 6297379555503105007, 2473663400150304794, 18362433840513668572];
        let b = 17275533833223164845;
        let borrow = submul_nx1(&mut lhs, &a, b);
        assert_eq!(
            lhs,
            [2427453526388035261, 7389014268281543265, 6670181329660292018, 8411211985208067428]
        );
        assert_eq!(borrow, 17196576577663999042);
    }
}
