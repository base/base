//! Block and address keys used by state history.
use core::ops::{Range, RangeInclusive};

use alloy_primitives::{Address, BlockNumber};
use serde::{Deserialize, Serialize};
/// [`BlockNumber`] concatenated with [`Address`].
///
/// Since it's used as a key, it isn't compressed when encoding it.
#[derive(
    Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Ord, PartialOrd, Hash,
)]
pub struct BlockNumberAddress(pub (BlockNumber, Address));

impl BlockNumberAddress {
    /// Create a new Range from `start` to `end`
    ///
    /// Note: End is inclusive
    pub fn range(range: RangeInclusive<BlockNumber>) -> Range<Self> {
        (*range.start(), Address::ZERO).into()..(*range.end() + 1, Address::ZERO).into()
    }

    /// Return the block number
    pub const fn block_number(&self) -> BlockNumber {
        self.0.0
    }

    /// Return the address
    pub const fn address(&self) -> Address {
        self.0.1
    }

    /// Consumes `Self` and returns [`BlockNumber`], [`Address`]
    pub const fn take(self) -> (BlockNumber, Address) {
        (self.0.0, self.0.1)
    }
}

impl From<(BlockNumber, Address)> for BlockNumberAddress {
    fn from(tpl: (u64, Address)) -> Self {
        Self(tpl)
    }
}

#[cfg(any(test, feature = "arbitrary"))]
impl<'a> arbitrary::Arbitrary<'a> for BlockNumberAddress {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> Result<Self, arbitrary::Error> {
        let mut bytes = [0; 28];
        input.fill_buffer(&mut bytes)?;
        let number = u64::from_be_bytes(bytes[..8].try_into().expect("eight-byte block number"));
        Ok(Self((number, Address::from_slice(&bytes[8..]))))
    }
}
