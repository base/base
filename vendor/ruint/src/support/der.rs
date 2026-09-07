//! Support for the [`der`](https://crates.io/crates/der) crate.
#![cfg(feature = "der")]
#![cfg_attr(docsrs, doc(cfg(feature = "der")))]

use alloc::boxed::Box;
use core::cmp::Ordering;

use der::{
    DecodeValue, EncodeValue, Error, FixedTag, Header, Length, Reader, Result, Tag, ValueOrd,
    Writer,
    asn1::{Any, AnyRef, Int, IntRef, Uint as DerUint, UintRef},
};

use crate::Uint;

impl<const BITS: usize, const LIMBS: usize> ValueOrd for Uint<BITS, LIMBS> {
    fn value_cmp(&self, other: &Self) -> Result<Ordering> {
        // DER encoding corresponds to integer comparison.
        Ok(self.cmp(other))
    }
}

impl<const BITS: usize, const LIMBS: usize> FixedTag for Uint<BITS, LIMBS> {
    const TAG: Tag = Tag::Integer;
}

impl<const BITS: usize, const LIMBS: usize> EncodeValue for Uint<BITS, LIMBS> {
    fn value_len(&self) -> Result<Length> {
        (1 + self.bit_len() / 8).try_into()
    }

    fn encode_value(&self, writer: &mut impl Writer) -> Result<()> {
        // Write bytes in big-endian order without leading zeros.
        let bytes = self.to_be_bytes_trimmed_vec();
        // Add leading `0x00` byte if the first byte has the highest bit set.
        // or if the sequence is empty.
        if bytes.first().copied().unwrap_or(0x80) >= 0x80 {
            writer.write_byte(0x00)?;
        }
        writer.write(&bytes)
    }
}

impl<'a, const BITS: usize, const LIMBS: usize> DecodeValue<'a> for Uint<BITS, LIMBS> {
    fn decode_value<R: Reader<'a>>(reader: &mut R, header: Header) -> Result<Self> {
        if header.length > Length::try_from(Self::BYTES + 1)? {
            return Err(Self::TAG.non_canonical_error());
        }
        from_der_slice(reader.read_vec(header.length)?.as_slice())
    }
}

impl<const BITS: usize, const LIMBS: usize> TryFrom<AnyRef<'_>> for Uint<BITS, LIMBS> {
    type Error = Error;

    fn try_from(any: AnyRef<'_>) -> Result<Self> {
        any.decode_as()
    }
}

impl<const BITS: usize, const LIMBS: usize> TryFrom<IntRef<'_>> for Uint<BITS, LIMBS> {
    type Error = Error;

    fn try_from(int: IntRef<'_>) -> Result<Self> {
        from_der_slice(int.as_bytes())
    }
}

impl<const BITS: usize, const LIMBS: usize> TryFrom<UintRef<'_>> for Uint<BITS, LIMBS> {
    type Error = Error;

    fn try_from(uint: UintRef<'_>) -> Result<Self> {
        from_der_uint_slice(uint.as_bytes())
    }
}

impl<const BITS: usize, const LIMBS: usize> TryFrom<&Any> for Uint<BITS, LIMBS> {
    type Error = Error;

    fn try_from(any: &Any) -> Result<Self> {
        any.decode_as()
    }
}

impl<const BITS: usize, const LIMBS: usize> TryFrom<&Int> for Uint<BITS, LIMBS> {
    type Error = Error;

    fn try_from(int: &Int) -> Result<Self> {
        from_der_slice(int.as_bytes())
    }
}

impl<const BITS: usize, const LIMBS: usize> TryFrom<&DerUint> for Uint<BITS, LIMBS> {
    type Error = Error;

    fn try_from(uint: &DerUint) -> Result<Self> {
        from_der_uint_slice(uint.as_bytes())
    }
}

// `Any::new()` only returns error when length > u32:MAX, which is out of scope
// for Uint.
#[allow(clippy::fallible_impl_from)]
impl<const BITS: usize, const LIMBS: usize> From<&Uint<BITS, LIMBS>> for Any {
    fn from(uint: &Uint<BITS, LIMBS>) -> Self {
        if uint.is_zero() {
            Self::new(Tag::Integer, Box::new([0_u8]) as Box<[u8]>).unwrap()
        } else {
            let mut bytes = uint.to_be_bytes_trimmed_vec();
            if bytes[0] >= 0x80 {
                bytes.insert(0, 0);
            }
            Self::new(Tag::Integer, bytes).unwrap()
        }
    }
}

// `Int::new()` only returns error when length > u32:MAX, which is out of scope
// for Uint.
#[allow(clippy::fallible_impl_from)]
impl<const BITS: usize, const LIMBS: usize> From<&Uint<BITS, LIMBS>> for Int {
    fn from(uint: &Uint<BITS, LIMBS>) -> Self {
        if uint.is_zero() {
            Self::new(&[0]).unwrap()
        } else {
            let mut bytes = uint.to_be_bytes_trimmed_vec();
            if bytes[0] >= 0x80 {
                bytes.insert(0, 0);
            }
            Self::new(&bytes).unwrap()
        }
    }
}

// `DerUint::new()` only returns error when length > u32:MAX, which is out of
// scope for Uint.
#[allow(clippy::fallible_impl_from)]
impl<const BITS: usize, const LIMBS: usize> From<&Uint<BITS, LIMBS>> for DerUint {
    fn from(uint: &Uint<BITS, LIMBS>) -> Self {
        if uint.is_zero() {
            Self::new(&[0]).unwrap()
        } else {
            // Panics:
            // The only error is if the length is more than can be represented in u32.
            // This is well outside of the inteded usecase for this library.
            Self::new(&uint.to_be_bytes_trimmed_vec()).unwrap()
        }
    }
}

macro_rules! forward_ref {
    ($ty:ty) => {
        impl<const BITS: usize, const LIMBS: usize> TryFrom<$ty> for Uint<BITS, LIMBS> {
            type Error = Error;

            fn try_from(obj: $ty) -> Result<Self> {
                Self::try_from(&obj)
            }
        }

        impl<const BITS: usize, const LIMBS: usize> From<Uint<BITS, LIMBS>> for $ty {
            fn from(uint: Uint<BITS, LIMBS>) -> Self {
                <$ty>::from(&uint)
            }
        }
    };
}

forward_ref!(Any);
forward_ref!(Int);
forward_ref!(DerUint);

fn from_der_slice<const BITS: usize, const LIMBS: usize>(
    bytes: &[u8],
) -> Result<Uint<BITS, LIMBS>> {
    // Handle sign bits and zero-prefix.
    let bytes = match bytes {
        [] => Err(Tag::Integer.length_error()),
        [0, byte, ..] if *byte < 0x80 => Err(Tag::Integer.non_canonical_error()),
        [0, rest @ ..] => Ok(rest),
        [byte, ..] if *byte >= 0x80 => Err(Tag::Integer.value_error()),
        bytes => Ok(bytes),
    }?;
    Uint::try_from_be_slice(bytes).ok_or_else(|| Tag::Integer.non_canonical_error())
}

fn from_der_uint_slice<const BITS: usize, const LIMBS: usize>(
    bytes: &[u8],
) -> Result<Uint<BITS, LIMBS>> {
    // UintRef and Uint have the leading 0x00 removed.
    match bytes {
        [] => Err(Tag::Integer.length_error()),
        [0] => Ok(Uint::ZERO),
        [0, ..] => Err(Tag::Integer.non_canonical_error()),
        bytes => Uint::try_from_be_slice(bytes).ok_or_else(|| Tag::Integer.non_canonical_error()),
    }
}

#[cfg(test)]
mod tests {
    use der::{Decode, Encode};
    use proptest::proptest;

    use super::*;
    use crate::{const_for, nlimbs};

    #[test]
    fn test_der_roundtrip() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            proptest!(|(value: Uint<BITS, LIMBS>)| {
                let serialized = value.to_der().unwrap();
                let deserialized = Uint::from_der(&serialized).unwrap();
                assert_eq!(value, deserialized);
            });
        });
    }

    #[test]
    fn test_u128_equiv() {
        proptest!(|(value: u128)| {
            let uint = Uint::<128, 2>::from(value);
            let serialized1 = value.to_der().unwrap();
            let serialized2 = uint.to_der().unwrap();
            assert_eq!(serialized1, serialized2);
        });
    }

    macro_rules! test_roundtrip {
        ($name:ident, $ty:ty) => {
            #[test]
            fn $name() {
                const_for!(BITS in SIZES {
                    const LIMBS: usize = nlimbs(BITS);
                    proptest!(|(value: Uint<BITS, LIMBS>)| {
                        let serialized = value.to_der().unwrap();
                        let der = <$ty>::from_der(&serialized).unwrap();
                        let deserialized = Uint::<BITS, LIMBS>::try_from(der).unwrap();
                        assert_eq!(value, deserialized);
                    });
                });
            }
        }
    }

    test_roundtrip!(test_der_anyref_roundtrip, AnyRef);
    test_roundtrip!(test_der_intref_roundtrip, IntRef);
    test_roundtrip!(test_der_uintref_roundtrip, UintRef);
    test_roundtrip!(test_der_any_roundtrip, Any);
    test_roundtrip!(test_der_int_roundtrip, Int);
    test_roundtrip!(test_der_uint_roundtrip, DerUint);

    macro_rules! test_into {
        ($name:ident, $ty:ty) => {
            #[test]
            fn $name() {
                const_for!(BITS in SIZES {
                    const LIMBS: usize = nlimbs(BITS);
                    proptest!(|(value: Uint<BITS, LIMBS>)| {
                        let obj: $ty = value.into();
                        let result: Uint<BITS, LIMBS> = obj.try_into().unwrap();
                        assert_eq!(result, value);
                    });
                });
            }
        }
    }

    test_into!(test_into_any, Any);
    test_into!(test_into_int, Int);
    test_into!(test_into_uint, DerUint);
}
