//! Support for the [`arbitrary`](https://crates.io/crates/arbitrary) crate.

#![cfg(feature = "arbitrary")]
#![cfg_attr(docsrs, doc(cfg(feature = "arbitrary")))]

use arbitrary::{Arbitrary, Result, Unstructured};

use crate::Uint;

// TODO: Instead of uniform random sampling, we should use a distribution that
// exercises different scales more. Something like sum(±2ⁱ for random i). The
// reduction step can then remove terms or make them smaller.

// TODO: We should use `rand` in tests, not `arbitrary`.

impl<'a, const BITS: usize, const LIMBS: usize> Arbitrary<'a> for Uint<BITS, LIMBS> {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let mut limbs = [0; LIMBS];
        if let Some((last, rest)) = limbs.split_last_mut() {
            for limb in rest {
                *limb = u64::arbitrary(u)?;
            }
            *last = u.int_in_range(0..=Self::MASK)?;
        }
        Ok(Self::from_limbs(limbs))
    }

    fn size_hint(_depth: usize) -> (usize, Option<usize>) {
        let bytes = BITS.div_ceil(8);
        (bytes, Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use alloc::vec::Vec;
    use core::iter::repeat_n;

    use super::*;
    use crate::{const_for, nlimbs};

    #[test]
    fn test_arbitrary() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            let (num_bytes, _) = Uint::<BITS, LIMBS>::size_hint(0);
            let bytes = repeat_n(0x55u8, num_bytes).collect::<Vec<_>>();
            let mut u = arbitrary::Unstructured::new(&bytes);
            Uint::<BITS, LIMBS>::arbitrary(&mut u).unwrap();
        });
    }
}
