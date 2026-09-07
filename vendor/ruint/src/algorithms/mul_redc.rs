// TODO: https://baincapitalcrypto.com/optimizing-montgomery-multiplication-in-webassembly/

use core::{cmp::Ordering, iter::zip};

use super::{DW, borrowing_sub, carrying_add, cmp};
use crate::utils::select_unpredictable;

/// ⚠️ Computes a * b * 2^(-BITS) mod modulus
#[doc = crate::algorithms::unstable_warning!()]
/// Requires that `inv` is the inverse of `-modulus[0]` modulo `2^64`.
/// Requires that `a` and `b` are less than `modulus`.
#[inline]
#[must_use]
pub fn mul_redc<const N: usize>(a: [u64; N], b: [u64; N], modulus: [u64; N], inv: u64) -> [u64; N] {
    debug_assert_eq!(inv.wrapping_mul(modulus[0]), u64::MAX);
    debug_assert_eq!(cmp(&a, &modulus), Ordering::Less);
    debug_assert_eq!(cmp(&b, &modulus), Ordering::Less);

    // Coarsely Integrated Operand Scanning (CIOS)
    // See <https://www.microsoft.com/en-us/research/wp-content/uploads/1998/06/97Acar.pdf>
    // See <https://hackmd.io/@gnark/modular_multiplication#fn1>
    // See <https://tches.iacr.org/index.php/TCHES/article/view/10972>
    let mut result = [0; N];
    let mut carry = false;
    for b in b {
        let mut m = 0;
        let mut carry_1 = 0;
        let mut carry_2 = 0;
        for i in 0..N {
            // Add limb product
            let (value, next_carry) = carrying_mul_add(a[i], b, result[i], carry_1);
            carry_1 = next_carry;

            if i == 0 {
                // Compute reduction factor
                m = value.wrapping_mul(inv);
            }

            // Add m * modulus to acc to clear next_result[0]
            let (value, next_carry) = carrying_mul_add(modulus[i], m, value, carry_2);
            carry_2 = next_carry;

            // Shift result
            if i > 0 {
                result[i - 1] = value;
            } else {
                debug_assert_eq!(value, 0);
            }
        }

        // Add carries
        let (value, next_carry) = carrying_add(carry_1, carry_2, carry);
        result[N - 1] = value;
        if modulus[N - 1] >= 0x7fff_ffff_ffff_ffff {
            carry = next_carry;
        } else {
            debug_assert!(!next_carry);
        }
    }

    // Compute reduced product.
    reduce1_carry(result, modulus, carry)
}

/// ⚠️ Computes a^2 * 2^(-BITS) mod modulus
#[doc = crate::algorithms::unstable_warning!()]
/// Requires that `inv` is the inverse of `-modulus[0]` modulo `2^64`.
/// Requires that `a` is less than `modulus`.
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn square_redc<const N: usize>(a: [u64; N], modulus: [u64; N], inv: u64) -> [u64; N] {
    debug_assert_eq!(inv.wrapping_mul(modulus[0]), u64::MAX);
    debug_assert_eq!(cmp(&a, &modulus), Ordering::Less);

    let mut result = [0; N];
    let mut carry_outer = 0;
    for i in 0..N {
        // Add limb product
        let (value, mut carry_lo) = carrying_mul_add(a[i], a[i], result[i], 0);
        let mut carry_hi = false;
        result[i] = value;
        for j in (i + 1)..N {
            let (value, next_carry_lo, next_carry_hi) =
                carrying_double_mul_add(a[i], a[j], result[j], carry_lo, carry_hi);
            result[j] = value;
            carry_lo = next_carry_lo;
            carry_hi = next_carry_hi;
        }

        // Add m times modulus to result and shift one limb
        let m = result[0].wrapping_mul(inv);
        let (value, mut carry) = carrying_mul_add(m, modulus[0], result[0], 0);
        debug_assert_eq!(value, 0);
        for j in 1..N {
            let (value, next_carry) = carrying_mul_add(modulus[j], m, result[j], carry);
            result[j - 1] = value;
            carry = next_carry;
        }

        // Add carries
        if modulus[N - 1] >= 0x3fff_ffff_ffff_ffff {
            let wide = (carry_outer as u128)
                .wrapping_add(carry_lo as u128)
                .wrapping_add((carry_hi as u128) << 64)
                .wrapping_add(carry as u128);
            result[N - 1] = wide as u64;

            // Note carry_outer can be {0, 1, 2}.
            carry_outer = (wide >> 64) as u64;
            debug_assert!(carry_outer <= 2);
        } else {
            // `carry_outer` and `carry_hi` are always zero.
            debug_assert!(!carry_hi);
            debug_assert_eq!(carry_outer, 0);
            let (value, carry) = carry_lo.overflowing_add(carry);
            debug_assert!(!carry);
            result[N - 1] = value;
        }
    }

    // Compute reduced product.
    debug_assert!(carry_outer <= 1);
    reduce1_carry(result, modulus, carry_outer > 0)
}

#[inline]
#[must_use]
#[allow(clippy::needless_bitwise_bool)]
fn reduce1_carry<const N: usize>(value: [u64; N], modulus: [u64; N], carry: bool) -> [u64; N] {
    let (reduced, borrow) = sub(value, modulus);
    select_unpredictable(carry | !borrow, reduced, value)
}

#[inline]
#[must_use]
fn sub<const N: usize>(lhs: [u64; N], rhs: [u64; N]) -> ([u64; N], bool) {
    let mut result = [0; N];
    let mut borrow = false;
    for (result, (lhs, rhs)) in zip(&mut result, zip(lhs, rhs)) {
        let (value, next_borrow) = borrowing_sub(lhs, rhs, borrow);
        *result = value;
        borrow = next_borrow;
    }
    (result, borrow)
}

/// Compute `lhs * rhs + add + carry`.
/// The output can not overflow for any input values.
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation)]
fn carrying_mul_add(lhs: u64, rhs: u64, add: u64, carry: u64) -> (u64, u64) {
    DW::split(DW::muladd2(lhs, rhs, add, carry))
}

/// Compute `2 * lhs * rhs + add + carry_lo + 2^64 * carry_hi`.
/// The output can not overflow for any input values.
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation)]
const fn carrying_double_mul_add(
    lhs: u64,
    rhs: u64,
    add: u64,
    carry_lo: u64,
    carry_hi: bool,
) -> (u64, u64, bool) {
    let wide = (lhs as u128).wrapping_mul(rhs as u128);
    let (wide, carry_1) = wide.overflowing_add(wide);
    let carries =
        (add as u128).wrapping_add(carry_lo as u128).wrapping_add((carry_hi as u128) << 64);
    let (wide, carry_2) = wide.overflowing_add(carries);
    (wide as u64, (wide >> 64) as u64, carry_1 | carry_2)
}

#[cfg(test)]
mod test {
    use core::ops::Neg;

    use proptest::{prop_assert_eq, proptest};

    use super::{
        super::{addmul, div},
        *,
    };
    use crate::{Uint, aliases::U64, const_for, nlimbs};

    fn modmul<const N: usize>(a: [u64; N], b: [u64; N], modulus: [u64; N]) -> [u64; N] {
        // Compute a * b
        let mut product = vec![0; 2 * N];
        addmul(&mut product, &a, &b);

        // Compute product mod modulus
        let mut reduced = modulus;
        div(&mut product, &mut reduced);
        reduced
    }

    fn mul_base<const N: usize>(a: [u64; N], modulus: [u64; N]) -> [u64; N] {
        // Compute a * 2^(N * 64)
        let mut product = vec![0; 2 * N];
        product[N..].copy_from_slice(&a);

        // Compute product mod modulus
        let mut reduced = modulus;
        div(&mut product, &mut reduced);
        reduced
    }

    #[test]
    fn test_mul_redc() {
        const_for!(BITS in NON_ZERO if BITS >= 16 {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(mut a: U, mut b: U, mut m: U)| {
                m |= U::from(1_u64); // Make sure m is odd.
                a %= m; // Make sure a is less than m.
                b %= m; // Make sure b is less than m.
                let a = *a.as_limbs();
                let b = *b.as_limbs();
                let m = *m.as_limbs();
                let inv = U64::from(m[0]).inv_ring().unwrap().neg().as_limbs()[0];

                let result = mul_base(mul_redc(a, b, m, inv), m);
                let expected = modmul(a, b, m);

                prop_assert_eq!(result, expected);
            });
        });
    }

    #[test]
    fn test_square_redc() {
        const_for!(BITS in NON_ZERO if BITS >= 16 {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(mut a: U, mut m: U)| {
                m |= U::from(1_u64); // Make sure m is odd.
                a %= m; // Make sure a is less than m.
                let a = *a.as_limbs();
                let m = *m.as_limbs();
                let inv = U64::from(m[0]).inv_ring().unwrap().neg().as_limbs()[0];

                let result = mul_base(square_redc(a, m, inv), m);
                let expected = modmul(a, a, m);

                prop_assert_eq!(result, expected);
            });
        });
    }
}
