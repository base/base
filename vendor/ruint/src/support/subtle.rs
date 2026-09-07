//! Support for the [`subtle`](https://crates.io/crates/subtle) crate.

#![cfg(feature = "subtle")]
#![cfg_attr(docsrs, doc(cfg(feature = "subtle")))]

use subtle::{
    Choice, ConditionallySelectable, ConstantTimeEq, ConstantTimeGreater, ConstantTimeLess,
};

use crate::Uint;

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// Returns a [`Choice`] if the bit at index is set.
    ///
    /// Constant time version of [`Uint::bit`]
    ///
    /// # Panics
    ///
    /// Panics if `index >= Self::BITS`.
    #[must_use]
    pub fn bit_ct(&self, index: usize) -> Choice {
        assert!(index < BITS);
        let (limbs, bits) = (index / 64, index % 64);
        (self.limbs[limbs] & (1 << bits)).ct_eq(&(1 << bits))
    }
}

impl<const BITS: usize, const LIMBS: usize> ConditionallySelectable for Uint<BITS, LIMBS> {
    fn conditional_select(a: &Self, b: &Self, choice: Choice) -> Self {
        let mut limbs = [0_u64; LIMBS];
        for (limb, (a, b)) in limbs.iter_mut().zip(a.as_limbs().iter().zip(b.as_limbs().iter())) {
            *limb = u64::conditional_select(a, b, choice);
        }
        Self::from_limbs(limbs)
    }
}

impl<const BITS: usize, const LIMBS: usize> ConstantTimeEq for Uint<BITS, LIMBS> {
    #[inline]
    fn ct_eq(&self, rhs: &Self) -> Choice {
        // Leverage ConstantTimeEq for &[u64]
        self.as_limbs().ct_eq(rhs.as_limbs())
    }
}

impl<const BITS: usize, const LIMBS: usize> ConstantTimeGreater for Uint<BITS, LIMBS> {
    fn ct_gt(&self, rhs: &Self) -> Choice {
        let mut equal = Choice::from(1); // True
        let mut greater = Choice::from(0); // False

        // Iterate limbs in big-endian order.
        for (l, r) in self.as_limbs().iter().rev().zip(rhs.as_limbs().iter().rev()) {
            greater |= equal & l.ct_gt(r);
            equal &= l.ct_eq(r);
        }
        greater
    }
}

impl<const BITS: usize, const LIMBS: usize> ConstantTimeLess for Uint<BITS, LIMBS> {
    fn ct_lt(&self, rhs: &Self) -> Choice {
        let mut equal = Choice::from(1); // True
        let mut less = Choice::from(0); // False

        // Iterate limbs in big-endian order.
        for (l, r) in self.as_limbs().iter().rev().zip(rhs.as_limbs().iter().rev()) {
            less |= equal & l.ct_lt(r);
            equal &= l.ct_eq(r);
        }
        less
    }
}

#[cfg(test)]
mod tests {
    use proptest::proptest;
    use subtle::ConditionallyNegatable;

    use super::*;
    use crate::{const_for, nlimbs};

    #[test]
    fn test_bit() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            proptest!(|(n: Uint<BITS, LIMBS>, i in 0..BITS)| {
                let r = n.bit_ct(i);
                let e = n.bit(i);
                assert_eq!(bool::from(r), e);
            });
        });
    }

    #[test]
    fn test_select() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U, c: bool)| {
                let choice = Choice::from(c as u8);
                let r = U::conditional_select(&a, &b, choice);
                let e = if c { b } else { a };
                assert_eq!(r, e);
            });
        });
    }

    #[test]
    fn test_negate() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, c: bool)| {
                let choice = Choice::from(c as u8);
                let mut r = a;
                r.conditional_negate(choice);
                let e = if c { -a } else { a };
                assert_eq!(r, e);
            });
        });
    }

    #[test]
    fn test_eq() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U)| {
                let r = a.ct_eq(&b);
                let e = a == b;
                assert_eq!(bool::from(r), e);
            });
        });
    }

    #[test]
    fn test_lt() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U)| {
                let r = a.ct_lt(&b);
                let e = a < b;
                assert_eq!(bool::from(r), e);
            });
        });
    }

    #[test]
    fn test_gt() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U)| {
                let r = a.ct_gt(&b);
                let e = a > b;
                assert_eq!(bool::from(r), e);
            });
        });
    }
}
