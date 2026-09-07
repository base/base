//! Support for the [`num-integer`](https://crates.io/crates/num-integer) crate.

#![cfg(feature = "num-integer")]

use num_integer::{ExtendedGcd, Integer};

use crate::Uint;

impl<const BITS: usize, const LIMBS: usize> Integer for Uint<BITS, LIMBS> {
    #[inline]
    #[track_caller]
    fn div_floor(&self, other: &Self) -> Self {
        Self::wrapping_div(*self, *other)
    }

    #[inline]
    #[track_caller]
    fn mod_floor(&self, other: &Self) -> Self {
        Self::wrapping_rem(*self, *other)
    }

    #[inline]
    fn gcd(&self, other: &Self) -> Self {
        <Self>::gcd(*self, *other)
    }

    #[inline]
    #[track_caller]
    fn lcm(&self, other: &Self) -> Self {
        <Self>::lcm(*self, *other).unwrap()
    }

    #[inline]
    fn is_multiple_of(&self, other: &Self) -> bool {
        if other.is_zero() {
            return self.is_zero();
        }
        *self % *other == Self::ZERO
    }

    #[inline]
    fn is_even(&self) -> bool {
        !self.bit(0)
    }

    #[inline]
    fn is_odd(&self) -> bool {
        self.bit(0)
    }

    #[inline]
    #[track_caller]
    fn div_rem(&self, other: &Self) -> (Self, Self) {
        <Self>::div_rem(*self, *other)
    }

    #[inline]
    #[track_caller]
    fn div_ceil(&self, other: &Self) -> Self {
        <Self>::div_ceil(*self, *other)
    }

    #[inline]
    #[track_caller]
    fn div_mod_floor(&self, other: &Self) -> (Self, Self) {
        // Same as `div_rem` for unsigned integers.
        <Self>::div_rem(*self, *other)
    }

    #[inline]
    fn extended_gcd(&self, other: &Self) -> ExtendedGcd<Self> {
        let (gcd, x, y, _sign) = <Self>::gcd_extended(*self, *other);
        ExtendedGcd { gcd, x, y }
    }

    #[inline]
    fn dec(&mut self) {
        *self -= Self::ONE;
    }

    #[inline]
    fn inc(&mut self) {
        *self += Self::ONE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_even() {
        let mut a = Uint::<64, 1>::from(0u32);
        for _ in 0..10 {
            a.inc();
            assert_eq!(a.is_even(), a.to::<u64>() % 2 == 0);
            assert_eq!(a.is_odd(), a.to::<u64>() % 2 != 0);
        }
    }
}
