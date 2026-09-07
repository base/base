use core::ops::{Div, DivAssign, Rem, RemAssign};

use crate::{Uint, algorithms};

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// Computes `self / rhs`, returning [`None`] if `rhs == 0`.
    #[inline]
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // False positive
    pub fn checked_div(self, rhs: Self) -> Option<Self> {
        if rhs.is_zero() {
            return None;
        }
        Some(self.div(rhs))
    }

    /// Computes `self / rhs`.
    ///
    /// # Panics
    ///
    /// This function will panic if `rhs == 0`.
    #[inline]
    #[must_use]
    #[track_caller]
    #[allow(clippy::missing_const_for_fn)] // False positive
    pub fn strict_div(self, rhs: Self) -> Self {
        match self.checked_div(rhs) {
            Some(value) => value,
            None => panic!("attempt to divide by zero"),
        }
    }

    /// Computes `self % rhs`, returning [`None`] if `rhs == 0`.
    #[inline]
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // False positive
    pub fn checked_rem(self, rhs: Self) -> Option<Self> {
        if rhs.is_zero() {
            return None;
        }
        Some(self.rem(rhs))
    }

    /// Computes `self % rhs`.
    ///
    /// # Panics
    ///
    /// This function will panic if `rhs == 0`.
    #[inline]
    #[must_use]
    #[track_caller]
    #[allow(clippy::missing_const_for_fn)] // False positive
    pub fn strict_rem(self, rhs: Self) -> Self {
        match self.checked_rem(rhs) {
            Some(value) => value,
            None => panic!("attempt to calculate the remainder with a divisor of zero"),
        }
    }

    /// Computes `self / rhs` rounding up.
    ///
    /// # Panics
    ///
    /// Panics if `rhs == 0`.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn div_ceil(self, rhs: Self) -> Self {
        let (q, r) = self.div_rem(rhs);
        if r.is_zero() { q } else { q + Self::ONE }
    }

    /// Computes `self / rhs` and `self % rhs`.
    ///
    /// # Panics
    ///
    /// Panics if `rhs == 0`.
    #[inline(always)]
    #[must_use]
    #[track_caller]
    pub fn div_rem(mut self, mut rhs: Self) -> (Self, Self) {
        if LIMBS == 1 {
            let q = &mut self.limbs[0];
            let r = &mut rhs.limbs[0];
            (*q, *r) = algorithms::div::div_1x1(*q, *r);
        } else {
            Self::div_rem_by_ref(&mut self, &mut rhs);
        }
        (self, rhs)
    }

    #[inline(never)]
    pub(crate) fn div_rem_by_ref(numerator: &mut Self, rhs: &mut Self) {
        algorithms::div::div_inlined(&mut numerator.limbs, &mut rhs.limbs);
    }

    /// Computes `self / rhs` rounding down.
    ///
    /// # Panics
    ///
    /// Panics if `rhs == 0`.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn wrapping_div(self, rhs: Self) -> Self {
        self.div_rem(rhs).0
    }

    /// Computes `self % rhs`.
    ///
    /// # Panics
    ///
    /// Panics if `rhs == 0`.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn wrapping_rem(self, rhs: Self) -> Self {
        self.div_rem(rhs).1
    }
}

impl_bin_op!(Div, div, DivAssign, div_assign, wrapping_div);
impl_bin_op!(Rem, rem, RemAssign, rem_assign, wrapping_rem);

#[cfg(test)]
mod tests {
    use proptest::{prop_assume, proptest};

    use super::*;
    use crate::{const_for, nlimbs};

    #[test]
    fn test_div_ceil() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(n: U, mut d: U)| {
                d >>= BITS / 2; // make d small
                prop_assume!(d != U::ZERO);
                let qf = n / d;
                let qc = n.div_ceil(d);
                assert!(qf <= qc);
                assert!(qf == qc || qf == qc - U::ONE);
                if qf == qc {
                    assert!(n % d == U::ZERO);
                }
            });
        });
    }

    #[test]
    fn test_divrem() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(n: U, mut d: u64)| {
                if BITS < 64 {
                    d &= U::MASK;
                }
                if d == 0 {
                    d = 1;
                }
                let d = U::from(d);
                let (q, r) = n.div_rem(d);
                assert!(r < d);
                assert_eq!(q * d + r, n);
            });
            proptest!(|(n: U, mut d: U)| {
                d >>= BITS / 2; // make d small
                prop_assume!(d != U::ZERO);
                let (q, r) = n.div_rem(d);
                assert!(r < d);
                assert_eq!(q * d + r, n);
            });
            proptest!(|(n: U, d: U)| {
                prop_assume!(d != U::ZERO);
                let (q, r) = n.div_rem(d);
                assert!(r < d);
                assert_eq!(q * d + r, n);
            });
        });
    }

    #[test]
    fn test_strict_div_rem_ok() {
        use crate::aliases::U64;
        assert_eq!(U64::from(7u64).strict_div(U64::from(2u64)), U64::from(3u64));
        assert_eq!(U64::from(7u64).strict_rem(U64::from(2u64)), U64::from(1u64));
    }

    #[test]
    #[should_panic(expected = "attempt to divide by zero")]
    fn test_strict_div_by_zero() {
        let _ = crate::aliases::U64::from(1u64).strict_div(crate::aliases::U64::ZERO);
    }

    #[test]
    #[should_panic(expected = "attempt to calculate the remainder with a divisor of zero")]
    fn test_strict_rem_by_zero() {
        let _ = crate::aliases::U64::from(1u64).strict_rem(crate::aliases::U64::ZERO);
    }
}
