use crate::{Uint, algorithms};

// FEATURE: sub_mod, neg_mod, inv_mod, div_mod, root_mod
// See <https://en.wikipedia.org/wiki/Cipolla's_algorithm>
// FEATURE: mul_mod_redc
// and maybe barrett
// See also <https://static1.squarespace.com/static/61f7cacf2d7af938cad5b81c/t/62deb4e0c434f7134c2730ee/1658762465114/modular_multiplication.pdf>
// FEATURE: Modular wrapper class, like Wrapping.

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// ⚠️ Compute $\mod{\mathtt{self}}_{\mathtt{modulus}}$.
    ///
    /// **Warning.** This function is not part of the stable API.
    ///
    /// Returns zero if the modulus is zero.
    // FEATURE: Reduce larger bit-sizes to smaller ones.
    #[inline]
    #[must_use]
    pub fn reduce_mod(mut self, modulus: Self) -> Self {
        if modulus.is_zero() {
            return Self::ZERO;
        }
        if self >= modulus {
            self %= modulus;
        }
        self
    }

    /// Compute $\mod{\mathtt{self} + \mathtt{rhs}}_{\mathtt{modulus}}$.
    ///
    /// Returns zero if the modulus is zero.
    #[inline]
    #[must_use]
    pub fn add_mod(mut self, rhs: Self, mut modulus: Self) -> Self {
        if modulus.is_zero() {
            return Self::ZERO;
        }

        // This is not going to truncate with the final cast because the modulus value
        // is 64 bits.
        #[allow(clippy::cast_possible_truncation)]
        if BITS <= 64 {
            self.limbs[0] =
                ((self.limbs[0] as u128 + rhs.limbs[0] as u128) % modulus.limbs[0] as u128) as u64;
            return self;
        }

        // do overflowing add, then check if we should divrem
        let (result, overflow) = self.overflowing_add(rhs);
        if overflow {
            // Add carry bit to the result. We might need an extra limb.
            let_double_bits!(numerator);
            let (limb, bit) = (BITS / 64, BITS % 64);
            let numerator = &mut numerator[..=limb];
            numerator[..LIMBS].copy_from_slice(result.as_limbs());
            numerator[limb] |= 1 << bit;

            // Reuse `div_rem` if we don't need an extra limb.
            if const { crate::nlimbs(BITS + 1) == LIMBS } {
                let numerator = unsafe { &mut *numerator.as_mut_ptr().cast::<Self>() };
                Self::div_rem_by_ref(numerator, &mut modulus);
            } else {
                Self::div_rem_bits_plus_one(numerator.as_mut_ptr(), &mut modulus);
            }

            modulus
        } else {
            result.reduce_mod(modulus)
        }
    }

    #[inline(never)]
    fn div_rem_bits_plus_one(numerator: *mut u64, modulus: &mut Self) {
        // TODO(dani): check if this is worth special casing over just using
        // div_rem_double_bits
        let numerator = unsafe { core::slice::from_raw_parts_mut(numerator, LIMBS + 1) };
        algorithms::div::div_inlined(numerator, &mut modulus.limbs);
    }

    /// Compute $\mod{\mathtt{self} ⋅ \mathtt{rhs}}_{\mathtt{modulus}}$.
    ///
    /// Returns zero if the modulus is zero.
    ///
    /// See [`mul_redc`](Self::mul_redc) for a faster variant at the cost of
    /// some pre-computation.
    #[inline(always)]
    #[must_use]
    pub fn mul_mod(self, rhs: Self, mut modulus: Self) -> Self {
        self.mul_mod_by_ref(&rhs, &mut modulus);
        modulus
    }

    #[inline(never)]
    fn mul_mod_by_ref(&self, rhs: &Self, modulus: &mut Self) {
        if modulus.is_zero() {
            return;
        }
        let_double_bits!(product);
        let overflow = algorithms::addmul(product, self.as_limbs(), rhs.as_limbs());
        debug_assert!(!overflow);
        Self::div_rem_double_bits(product, modulus);
    }

    #[inline]
    fn div_rem_double_bits(numerator: &mut [u64], modulus: &mut Self) {
        assume!(numerator.len() == crate::nlimbs(BITS * 2));
        algorithms::div::div_inlined(numerator, &mut modulus.limbs);
    }

    /// Compute $\mod{\mathtt{self}^{\mathtt{rhs}}}_{\mathtt{modulus}}$.
    ///
    /// Returns zero if the modulus is zero.
    #[inline]
    #[must_use]
    pub fn pow_mod(mut self, mut exp: Self, modulus: Self) -> Self {
        if BITS == 0 || modulus <= Self::ONE {
            return Self::ZERO;
        }

        // Exponentiation by squaring
        let mut result = Self::ONE;
        while exp > Self::ZERO {
            // Multiply by base
            if exp.limbs[0] & 1 == 1 {
                result = result.mul_mod(self, modulus);
            }

            // Square base
            self = self.mul_mod(self, modulus);
            exp >>= 1;
        }
        result
    }

    /// Compute $\mod{\mathtt{self}^{-1}}_{\mathtt{modulus}}$.
    ///
    /// Returns `None` if the inverse does not exist.
    #[inline]
    #[must_use]
    pub fn inv_mod(self, modulus: Self) -> Option<Self> {
        algorithms::inv_mod(self, modulus)
    }

    /// Montgomery multiplication.
    ///
    /// Requires `self` and `other` to be less than `modulus`.
    ///
    /// Computes
    ///
    /// $$
    /// \mod{\frac{\mathtt{self} ⋅ \mathtt{other}}{ 2^{64 ·
    /// \mathtt{LIMBS}}}}_{\mathtt{modulus}} $$
    ///
    /// This is useful because it can be computed notably faster than
    /// [`mul_mod`](Self::mul_mod). Many computations can be done by
    /// pre-multiplying values with $R = 2^{64 · \mathtt{LIMBS}}$
    /// and then using [`mul_redc`](Self::mul_redc) instead of
    /// [`mul_mod`](Self::mul_mod).
    ///
    /// For this algorithm to work, it needs an extra parameter `inv` which must
    /// be set to
    ///
    /// $$
    /// \mathtt{inv} = \mod{\frac{-1}{\mathtt{modulus}} }_{2^{64}}
    /// $$
    ///
    /// The `inv` value only exists for odd values of `modulus`. It can be
    /// computed using [`inv_ring`](Self::inv_ring) from `U64`.
    ///
    /// ```
    /// # use ruint::{uint, Uint, aliases::*};
    /// # uint!{
    /// # let modulus = 21888242871839275222246405745257275088548364400416034343698204186575808495617_U256;
    /// let inv = U64::wrapping_from(modulus).inv_ring().unwrap().wrapping_neg().to();
    /// let prod = 5_U256.mul_redc(6_U256, modulus, inv);
    /// # assert_eq!(inv.wrapping_mul(modulus.wrapping_to()), u64::MAX);
    /// # assert_eq!(inv, 0xc2e1f593efffffff);
    /// # }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `inv` is not correct in debug mode.
    #[inline]
    #[must_use]
    pub fn mul_redc(self, other: Self, modulus: Self, inv: u64) -> Self {
        if BITS == 0 {
            return Self::ZERO;
        }
        let result = algorithms::mul_redc(self.limbs, other.limbs, modulus.limbs, inv);
        let result = Self::from_limbs(result);
        debug_assert!(result < modulus);
        result
    }

    /// Montgomery squaring.
    ///
    /// See [Self::mul_redc].
    #[inline]
    #[must_use]
    pub fn square_redc(self, modulus: Self, inv: u64) -> Self {
        if BITS == 0 {
            return Self::ZERO;
        }
        let result = algorithms::square_redc(self.limbs, modulus.limbs, inv);
        let result = Self::from_limbs(result);
        debug_assert!(result < modulus);
        result
    }
}

#[cfg(test)]
mod tests {
    use proptest::{prop_assume, proptest, test_runner::Config};

    use super::*;
    use crate::{aliases::U64, const_for, nlimbs};

    #[test]
    fn test_commutative() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U, m: U)| {
                assert_eq!(a.mul_mod(b, m), b.mul_mod(a, m));
            });
        });
    }

    #[test]
    fn test_associative() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U, c: U, m: U)| {
                assert_eq!(a.mul_mod(b.mul_mod(c, m), m), a.mul_mod(b, m).mul_mod(c, m));
            });
        });
    }

    #[test]
    fn test_distributive() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U, c: U, m: U)| {
                assert_eq!(a.mul_mod(b.add_mod(c, m), m), a.mul_mod(b, m).add_mod(a.mul_mod(c, m), m));
            });
        });
    }

    #[test]
    fn test_add_identity() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(value: U, m: U)| {
                assert_eq!(value.add_mod(U::from(0), m), value.reduce_mod(m));
            });
        });
    }

    #[test]
    fn test_mul_identity() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(value: U, m: U)| {
                assert_eq!(value.mul_mod(U::from(0), m), U::ZERO);
                assert_eq!(value.mul_mod(U::from(1), m), value.reduce_mod(m));
            });
        });
    }

    #[test]
    fn test_pow_identity() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, m: U)| {
                assert_eq!(a.pow_mod(U::from(0), m), U::from(1).reduce_mod(m));
                assert_eq!(a.pow_mod(U::from(1), m), a.reduce_mod(m));
            });
        });
    }

    #[test]
    fn test_pow_rules() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;

            // Too slow.
            if LIMBS > 8 {
                return;
            }

            let config = Config { cases: 5, ..Default::default() };
            proptest!(config, |(a: U, b: U, c: U, m: U)| {
                // TODO: a^(b+c) = a^b * a^c. Which requires carmichael fn.
                // TODO: (a^b)^c = a^(b * c). Which requires carmichael fn.
                assert_eq!(a.mul_mod(b, m).pow_mod(c, m), a.pow_mod(c, m).mul_mod(b.pow_mod(c, m), m));
            });
        });
    }

    #[test]
    fn test_inv() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, m: U)| {
                if let Some(inv) = a.inv_mod(m) {
                    assert_eq!(a.mul_mod(inv, m), U::from(1));
                }
            });
        });
    }

    #[test]
    fn test_mul_redc() {
        const_for!(BITS in NON_ZERO if BITS >= 16 {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, b: U, m: U)| {
                prop_assume!(m >= U::from(2));
                if let Some(inv) = U64::from(m.as_limbs()[0]).inv_ring() {
                    let inv = (-inv).as_limbs()[0];

                    let r = U::from(2).pow_mod(U::from(64 * LIMBS), m);
                    let ar = a.mul_mod(r, m);
                    let br = b.mul_mod(r, m);
                    // TODO: Test for larger (>= m) values of a, b.

                    let expected = a.mul_mod(b, m).mul_mod(r, m);

                    assert_eq!(ar.mul_redc(br, m, inv), expected);
                }
            });
        });
    }

    #[test]
    fn test_square_redc() {
        const_for!(BITS in NON_ZERO if BITS >= 16 {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint<BITS, LIMBS>;
            proptest!(|(a: U, m: U)| {
                prop_assume!(m >= U::from(2));
                if let Some(inv) = U64::from(m.as_limbs()[0]).inv_ring() {
                    let inv = (-inv).as_limbs()[0];

                    let r = U::from(2).pow_mod(U::from(64 * LIMBS), m);
                    let ar = a.mul_mod(r, m);
                    // TODO: Test for larger (>= m) values of a, b.

                    let expected = a.mul_mod(a, m).mul_mod(r, m);

                    assert_eq!(ar.square_redc(m, inv), expected);
                }
            });
        });
    }
}
