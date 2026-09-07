//! Support for the [`rand`](https://crates.io/crates/rand) crate.

#![cfg(feature = "rand")]
#![cfg_attr(docsrs, doc(cfg(feature = "rand")))]

// FEATURE: Implement the Uniform distribution.

use rand::{
    Rng,
    distributions::{Distribution, Standard},
};
use rand_08 as rand;

use crate::Uint;

impl<const BITS: usize, const LIMBS: usize> Distribution<Uint<BITS, LIMBS>> for Standard {
    #[inline]
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Uint<BITS, LIMBS> {
        <Uint<BITS, LIMBS>>::random_with_impl(rng)
    }
}

#[cfg(not(feature = "rand-09"))]
impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// Creates a new [`Uint`] with the default cryptographic random number
    /// generator.
    ///
    /// This is currently [`rand::thread_rng`].
    #[inline]
    #[must_use]
    #[cfg(feature = "std")]
    pub fn random() -> Self {
        let mut uint = Self::ZERO;
        uint.randomize();
        uint
    }

    /// Creates a new [`Uint`] with the given random number generator.
    #[inline]
    #[doc(alias = "random_using")]
    #[must_use]
    pub fn random_with<R: rand::RngCore + ?Sized>(rng: &mut R) -> Self {
        Self::random_with_impl(rng)
    }

    /// Fills this [`Uint`] with the default cryptographic random number
    /// generator.
    ///
    /// See [`random`](Self::random) for more details.
    #[inline]
    #[cfg(feature = "std")]
    pub fn randomize(&mut self) {
        self.randomize_with(&mut rand::thread_rng());
    }

    /// Fills this [`Uint`] with the given random number generator.
    #[inline]
    #[doc(alias = "randomize_using")]
    pub fn randomize_with<R: rand::RngCore + ?Sized>(&mut self, rng: &mut R) {
        self.randomize_with_impl(rng);
    }
}

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    #[inline]
    fn random_with_impl<R: rand::RngCore + ?Sized>(rng: &mut R) -> Self {
        let mut uint = Self::ZERO;
        uint.randomize_with_impl(rng);
        uint
    }

    #[inline]
    fn randomize_with_impl<R: rand::RngCore + ?Sized>(&mut self, rng: &mut R) {
        rng.fill(&mut self.limbs[..]);
        self.apply_mask();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{const_for, nlimbs};

    #[test]
    fn test_rand() {
        let mut rng = rand::thread_rng();
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            for _ in 0..1000 {
                let _: Uint<BITS, LIMBS> = rng.r#gen();
            }
        });
    }
}
