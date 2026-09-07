//! Support for the [`borsh`](https://crates.io/crates/borsh) crate.

#![cfg(feature = "borsh")]
#![cfg_attr(docsrs, doc(cfg(feature = "borsh")))]

use borsh::{BorshDeserialize, BorshSerialize, io};

use crate::{Bits, Uint};

impl<const BITS: usize, const LIMBS: usize> BorshDeserialize for Uint<BITS, LIMBS> {
    #[inline]
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        // This is a bit of an end-run around missing `generic_const_exprs`
        // We cannot declare a `[u8; Self::BYTES]` or `[u8; LIMBS * 8]`,
        // so we declare a `[u8; LIMBS]` and use unsafe to write to it.

        // TODO: Replace the unsafety with `generic_const_exprs` when
        // available
        let mut limbs = [0u64; LIMBS];

        // SAFETY: `limbs` is known to have identical memory layout and
        // alignment to `[u8; LIMBS * 8]`, which is guaranteed to safely
        // contain  [u8; Self::BYTES]`, as `LIMBS * 8 >= Self::BYTES`.
        // Reference:
        // https://doc.rust-lang.org/reference/type-layout.html#array-layout
        let target = unsafe {
            core::slice::from_raw_parts_mut(limbs.as_mut_ptr().cast::<u8>(), Self::BYTES)
        };
        reader.read_exact(target)?;

        // Using `Self::from_limbs(limbs)` would be incorrect here, as the
        // inner u64s are encoded in LE, and the platform may be BE.
        Self::try_from_le_slice(target).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "value is too large for the type")
        })
    }
}

impl<const BITS: usize, const LIMBS: usize> BorshSerialize for Uint<BITS, LIMBS> {
    #[inline]
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        #[cfg(target_endian = "little")]
        return writer.write_all(self.as_le_slice());

        // TODO: Replace the unsafety with `generic_const_exprs` when
        // available
        #[cfg(target_endian = "big")]
        {
            let mut limbs = [0u64; LIMBS];
            // SAFETY: `limbs` is known to have identical memory layout and
            // alignment to `[u8; LIMBS * 8]`, which is guaranteed to safely
            // contain  [u8; Self::BYTES]`, as `LIMBS * 8 >= Self::BYTES`.
            // Reference:
            // https://doc.rust-lang.org/reference/type-layout.html#array-layout
            let mut buf = unsafe {
                core::slice::from_raw_parts_mut(limbs.as_mut_ptr().cast::<u8>(), Self::BYTES)
            };
            self.copy_le_bytes_to(&mut buf);
            writer.write_all(&buf)
        }
    }
}

impl<const BITS: usize, const LIMBS: usize> BorshDeserialize for Bits<BITS, LIMBS> {
    #[inline]
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        Uint::<BITS, LIMBS>::deserialize_reader(reader).map(Into::into)
    }
}

impl<const BITS: usize, const LIMBS: usize> BorshSerialize for Bits<BITS, LIMBS> {
    #[inline]
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        self.as_uint().serialize(writer)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Debug, BorshDeserialize, BorshSerialize, PartialEq, Eq)]
    struct Something {
        is_it: bool,
        value: Uint<256, 4>,
    }

    #[test]
    fn test_uint() {
        let something = Something { is_it: true, value: Uint::<256, 4>::from_limbs([1, 2, 3, 4]) };
        let mut buf = [0; 33];

        something.serialize(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(
            buf,
            [
                1, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0,
                0, 0, 0, 0, 0
            ]
        );
        assert_eq!(&something.value.to_le_bytes::<32>(), &buf[1..]);
        assert_eq!(Something::try_from_slice(&buf[..]).unwrap(), something);
    }

    #[derive(Debug, BorshDeserialize, BorshSerialize, PartialEq, Eq)]
    struct AnotherThing {
        is_it: bool,
        value: Bits<256, 4>,
    }

    #[test]
    fn test_bits() {
        let another_thing =
            AnotherThing { is_it: true, value: Bits::<256, 4>::from_limbs([1, 2, 3, 4]) };
        let mut buf = [0; 33];

        another_thing.serialize(&mut buf.as_mut_slice()).unwrap();

        assert_eq!(
            buf,
            [
                1, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0,
                0, 0, 0, 0, 0
            ]
        );
        assert_eq!(&another_thing.value.to_le_bytes::<32>(), &buf[1..]);
        assert_eq!(AnotherThing::try_from_slice(&buf[..]).unwrap(), another_thing);
    }

    #[test]
    fn deser_invalid_value() {
        let buf = [0xff; 4];
        let mut reader = &mut &buf[..];

        let result = Uint::<31, 1>::deserialize_reader(&mut reader);
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "value is too large for the type");
    }

    #[derive(Debug, BorshDeserialize, BorshSerialize, PartialEq, Eq)]
    struct AThirdThing {
        value: Uint<64, 1>,
        bool_value: bool,
    }

    #[test]
    fn roundtrip_trailing_zeroes() {
        let instance = AThirdThing { value: Uint::<64, 1>::from_limbs([1]), bool_value: true };

        let mut buf = [0u8; 9];

        instance.serialize(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(buf, [1, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(&instance.value.to_le_bytes::<8>(), &buf[..8]);
        assert_eq!(AThirdThing::try_from_slice(&buf[..]).unwrap(), instance);
    }
}
