//! Support for the [`bigdecimal`](https://crates.io/crates/bigdecimal) crate.

#![cfg(feature = "bigdecimal")]
#![cfg_attr(docsrs, doc(cfg(feature = "bigdecimal")))]

use bigdecimal::BigDecimal;

use crate::{ToUintError, Uint};

impl<const BITS: usize, const LIMBS: usize> TryFrom<BigDecimal> for Uint<BITS, LIMBS> {
    type Error = ToUintError<Self>;

    fn try_from(value: BigDecimal) -> Result<Self, Self::Error> {
        Self::try_from(value.round(0).into_bigint_and_scale().0)
    }
}

impl<const BITS: usize, const LIMBS: usize> From<Uint<BITS, LIMBS>> for BigDecimal {
    fn from(value: Uint<BITS, LIMBS>) -> Self {
        Self::from_biguint(value.into(), 0)
    }
}
