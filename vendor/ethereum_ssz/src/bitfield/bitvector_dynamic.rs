//! Provides `Bitfield<Dynamic>` (BitVectorDynamic)
use core::marker::PhantomData;

use serde::{
    de::{Deserialize, Deserializer},
    ser::{Serialize, Serializer},
};
use serde_utils::hex::{PrefixedHexVisitor, encode as hex_encode};
use smallvec::{SmallVec, ToSmallVec, smallvec};

/// for encoding and decoding bitvectors that have a dynamic length.
use crate::{
    Decode, DecodeError, Encode,
    bitfield::{Bitfield, BitfieldBehaviour, Error, SMALLVEC_LEN, bytes_for_bit_len},
};

/// A marker struct used to declare dynamic length behaviour on a Bitfield.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dynamic;

/// A heap-allocated, ordered collection of `bool` values with a length set at runtime.
pub type BitVectorDynamic = Bitfield<Dynamic>;

impl BitfieldBehaviour for Dynamic {}

impl Bitfield<Dynamic> {
    /// Create a new dynamic bitfield with the given length (all bits false).
    /// Length must be a multiple of 8 bits and greater than 0.
    pub fn new(len: usize) -> Result<Self, Error> {
        if len == 0 {
            return Err(Error::InvalidByteCount { given: len, expected: 8 });
        }
        if !len.is_multiple_of(8) {
            return Err(Error::InvalidByteCount { given: len, expected: (len / 8 + 1) * 8 });
        }
        Ok(Self { bytes: smallvec![0; bytes_for_bit_len(len)], len, _phantom: PhantomData })
    }

    pub fn into_bytes(self) -> SmallVec<[u8; SMALLVEC_LEN]> {
        self.into_raw_bytes()
    }

    /// Create a dynamic bitfield from raw bytes and a declared logical length.
    /// Can be used to specify a max_length if called directly instead of via Decode.
    pub fn from_bytes_with_len(
        bytes: SmallVec<[u8; SMALLVEC_LEN]>,
        len: usize,
    ) -> Result<Self, Error> {
        if len != bytes.len() * 8 {
            return Err(Error::InvalidByteCount { given: len, expected: bytes.len() * 8 });
        }
        Self::from_raw_bytes(bytes, len)
    }

    /// Compute the intersection of two bitfields.
    pub fn intersection(&self, other: &Self) -> Result<Self, Error> {
        let max_len = std::cmp::max(self.len(), other.len());
        let mut result = Self::new(max_len)?;

        for (i, byte) in result.bytes.iter_mut().enumerate() {
            *byte =
                self.bytes.get(i).copied().unwrap_or(0) & other.bytes.get(i).copied().unwrap_or(0);
        }
        Ok(result)
    }

    /// Compute the union of two bitfields.
    pub fn union(&self, other: &Self) -> Result<Self, Error> {
        let max_len = std::cmp::max(self.len(), other.len());
        let mut result = Self::new(max_len)?;

        for (i, byte) in result.bytes.iter_mut().enumerate() {
            *byte =
                self.bytes.get(i).copied().unwrap_or(0) | other.bytes.get(i).copied().unwrap_or(0);
        }
        Ok(result)
    }
}

impl Encode for Bitfield<Dynamic> {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_bytes_len(&self) -> usize {
        self.bytes.len()
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.bytes)
    }
}

impl Decode for Bitfield<Dynamic> {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.is_empty() {
            return Err(DecodeError::BytesInvalid("Empty bytes".into()));
        }

        let len = bytes.len() * 8;
        Self::from_raw_bytes(bytes.to_smallvec(), len).map_err(|e| {
            DecodeError::BytesInvalid(format!("BitVectorDynamic failed to decode: {:?}", e))
        })
    }
}

impl Serialize for Bitfield<Dynamic> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex_encode(self.as_ssz_bytes()))
    }
}

impl<'de> Deserialize<'de> for Bitfield<Dynamic> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = deserializer.deserialize_str(PrefixedHexVisitor)?;
        Self::from_ssz_bytes(&bytes)
            .map_err(|e| serde::de::Error::custom(format!("BitVectorDynamic {:?}", e)))
    }
}

#[cfg(test)]
mod dynamic_bitfield_tests {
    use super::*;

    #[test]
    fn test_basic_operations() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(16)?;

        // Set/get within bounds should succeed or return Ok
        assert!(bitfield.set(0, true).is_ok());
        assert!(bitfield.set(15, true).is_ok());
        assert!(bitfield.set(16, true).is_err()); // Out of bounds

        assert_eq!(bitfield.get(0)?, true);
        assert_eq!(bitfield.get(15)?, true);
        assert!(bitfield.get(16).is_err());

        Ok(())
    }

    #[test]
    fn test_ssz_encode_decode() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(8)?;
        bitfield.set(0, true)?;
        bitfield.set(7, true)?;

        // Convert to raw bytes and decode with a known length
        let bytes = bitfield.clone().into_bytes();
        let decoded = BitVectorDynamic::from_bytes_with_len(bytes, 8)?;

        assert_eq!(bitfield, decoded);

        // Test invalid cases
        assert!(BitVectorDynamic::from_bytes_with_len(smallvec![], 8).is_err());
        assert!(BitVectorDynamic::from_bytes_with_len(smallvec![0, 0, 0], 8).is_err());

        Ok(())
    }

    #[test]
    fn test_ssz_decode_errors() {
        // Test empty bytes error
        let empty_bytes: &[u8] = &[];
        assert!(matches!(
            BitVectorDynamic::from_ssz_bytes(empty_bytes),
            Err(DecodeError::BytesInvalid(msg)) if msg == "Empty bytes"
        ));
    }

    #[test]
    fn test_intersection() -> Result<(), Error> {
        let mut a = BitVectorDynamic::new(16)?;
        let mut b = BitVectorDynamic::new(16)?;
        let mut expected = BitVectorDynamic::new(16)?;

        a.set(1, true)?;
        a.set(3, true)?;
        b.set(3, true)?;
        b.set(4, true)?;
        expected.set(3, true)?;

        assert_eq!(a.intersection(&b)?, expected);
        assert_eq!(b.intersection(&a)?, expected);

        Ok(())
    }

    #[test]
    fn test_union() -> Result<(), Error> {
        let mut a = BitVectorDynamic::new(16)?;
        let mut b = BitVectorDynamic::new(16)?;
        let mut expected = BitVectorDynamic::new(16)?;

        a.set(1, true)?;
        a.set(3, true)?;
        b.set(3, true)?;
        b.set(4, true)?;

        expected.set(1, true)?;
        expected.set(3, true)?;
        expected.set(4, true)?;

        assert_eq!(a.union(&b)?, expected);
        assert_eq!(b.union(&a)?, expected);

        Ok(())
    }

    #[test]
    fn test_highest_set_bit() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(16)?;
        assert_eq!(bitfield.highest_set_bit(), None);

        bitfield.set(3, true)?;
        assert_eq!(bitfield.highest_set_bit(), Some(3));

        bitfield.set(15, true)?;
        assert_eq!(bitfield.highest_set_bit(), Some(15));

        Ok(())
    }

    #[test]
    fn test_is_zero() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(16)?;
        assert!(bitfield.is_zero());

        bitfield.set(0, true)?;
        assert!(!bitfield.is_zero());

        bitfield.set(0, false)?;
        assert!(bitfield.is_zero());

        Ok(())
    }

    #[test]
    fn test_num_set_bits() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(16)?;
        assert_eq!(bitfield.num_set_bits(), 0);

        bitfield.set(1, true)?;
        bitfield.set(3, true)?;
        bitfield.set(7, true)?;
        assert_eq!(bitfield.num_set_bits(), 3);

        Ok(())
    }

    #[test]
    fn test_difference() -> Result<(), Error> {
        let mut a = BitVectorDynamic::new(16)?;
        let mut b = BitVectorDynamic::new(16)?;

        a.set(1, true)?;
        a.set(3, true)?;
        b.set(3, true)?;
        b.set(4, true)?;

        let diff = a.difference(&b);
        assert_eq!(diff.get(1)?, true);
        assert_eq!(diff.get(3)?, false);
        assert_eq!(diff.get(4)?, false);

        Ok(())
    }

    #[test]
    fn test_shift_up() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(16)?;
        bitfield.set(0, true)?;
        bitfield.set(1, true)?;

        bitfield.shift_up(1)?;
        assert_eq!(bitfield.get(0)?, false);
        assert_eq!(bitfield.get(1)?, true);
        assert_eq!(bitfield.get(2)?, true);

        // Test error case
        assert!(bitfield.shift_up(17).is_err());

        Ok(())
    }

    #[test]
    fn test_iter() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(8)?;
        bitfield.set(1, true)?;
        bitfield.set(4, true)?;
        bitfield.set(7, true)?;

        let bits: Vec<bool> = bitfield.iter().collect();
        assert_eq!(bits, vec![false, true, false, false, true, false, false, true]);

        Ok(())
    }

    #[test]
    fn test_non_byte_aligned_lengths() {
        // Zero bits should error
        assert!(BitVectorDynamic::new(0).is_err());

        // 1 bit should error
        assert!(BitVectorDynamic::new(1).is_err());

        // 7 bits should error
        assert!(BitVectorDynamic::new(7).is_err());

        // 8 bits should succeed
        assert!(BitVectorDynamic::new(8).is_ok());

        // 9 bits should error
        assert!(BitVectorDynamic::new(9).is_err());
    }

    #[test]
    fn test_encode_decode() -> Result<(), Error> {
        let mut bitfield = BitVectorDynamic::new(32)?;
        bitfield.set(0, true)?;
        bitfield.set(16, true)?;
        bitfield.set(31, true)?;

        let expected: SmallVec<[u8; 4]> =
            smallvec![0b0000_0001, 0b0000_0000, 0b0000_0001, 0b1000_0000];
        let bytes = bitfield.clone().into_bytes();
        assert_eq!(bytes, expected);

        let encoded = bitfield.as_ssz_bytes();
        let decoded = BitVectorDynamic::from_ssz_bytes(&encoded)
            .map_err(|_| Error::InvalidByteCount { given: encoded.len(), expected: bytes.len() })?
            .into_bytes();

        assert_eq!(bytes, decoded);

        Ok(())
    }

    #[test]
    fn test_from_bytes_equivalence() -> Result<(), Error> {
        let mut original = BitVectorDynamic::new(16)?;
        original.set(0, true)?;
        original.set(8, true)?;
        original.set(15, true)?;

        let bytes = original.clone().into_bytes();

        // Both methods should produce equivalent results
        let from_bytes = BitVectorDynamic::from_bytes_with_len(bytes.clone(), 16)?;
        let from_ssz = BitVectorDynamic::from_ssz_bytes(&bytes)
            .map_err(|_| Error::InvalidByteCount { given: 0, expected: 1 })?;

        assert_eq!(from_bytes, from_ssz);
        assert_eq!(from_bytes, original);

        Ok(())
    }

    #[test]
    fn test_from_bytes_length_validation() {
        // Create bytes representing a 32-bit field
        let bytes = smallvec![0b1111_1111; 4]; // 32 bits set

        // Trying to decode as 16 bits should fail
        assert!(matches!(
            BitVectorDynamic::from_bytes_with_len(bytes.clone(), 16),
            Err(Error::InvalidByteCount { .. })
        ));

        // Decoding with correct length should succeed
        assert!(BitVectorDynamic::from_bytes_with_len(bytes, 32).is_ok());
    }

    #[test]
    fn test_not() -> Result<(), Error> {
        // Test with all zeros -> all ones
        let a = BitVectorDynamic::new(8)?;
        let mut expected = BitVectorDynamic::new(8)?;
        for i in 0..8 {
            expected.set(i, true)?;
        }
        assert_eq!(a.not(), expected);

        // Test with all ones -> all zeros
        let b = expected.clone();
        assert_eq!(b.not(), BitVectorDynamic::new(8)?);

        // Test with mixed pattern (16 bits)
        let c = BitVectorDynamic::from_bytes_with_len(smallvec![0b1100_1010, 0b0011_0101], 16)?;
        let expected_c =
            BitVectorDynamic::from_bytes_with_len(smallvec![0b0011_0101, 0b1100_1010], 16)?;
        assert_eq!(c.not(), expected_c);

        // Test double not is identity
        let mut d = BitVectorDynamic::new(16)?;
        d.set(0, true)?;
        d.set(5, true)?;
        d.set(15, true)?;
        assert_eq!(d.not().not(), d);

        Ok(())
    }

    #[test]
    fn test_not_inplace() -> Result<(), Error> {
        // Test with all zeros
        let mut a = BitVectorDynamic::new(8)?;
        a.not_inplace();
        let mut expected = BitVectorDynamic::new(8)?;
        for i in 0..8 {
            expected.set(i, true)?;
        }
        assert_eq!(a, expected);

        // Test with all ones
        let mut b = expected.clone();
        b.not_inplace();
        assert_eq!(b, BitVectorDynamic::new(8)?);

        // Test with mixed pattern (16 bits)
        let mut c = BitVectorDynamic::from_bytes_with_len(smallvec![0b1100_1010, 0b0011_0101], 16)?;
        c.not_inplace();
        let expected_c =
            BitVectorDynamic::from_bytes_with_len(smallvec![0b0011_0101, 0b1100_1010], 16)?;
        assert_eq!(c, expected_c);

        // Test double not_inplace is identity
        let mut d = BitVectorDynamic::new(16)?;
        d.set(0, true)?;
        d.set(5, true)?;
        d.set(15, true)?;
        let original = d.clone();
        d.not_inplace();
        d.not_inplace();
        assert_eq!(d, original);

        Ok(())
    }

    #[test]
    fn test_ssz_bytes_len() -> Result<(), Error> {
        let bitfield = BitVectorDynamic::new(16)?; // 16 bits = 2 bytes
        assert_eq!(bitfield.ssz_bytes_len(), 2);

        let bitfield = BitVectorDynamic::new(24)?; // 24 bits = 3 bytes
        assert_eq!(bitfield.ssz_bytes_len(), 3);

        Ok(())
    }
}
#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    fn assert_round_trip_bitdyn<T>(t: T) -> Result<(), Error>
    where
        T: Encode + Decode + PartialEq + std::fmt::Debug,
    {
        let bytes = t.as_ssz_bytes();
        let decoded = T::from_ssz_bytes(&bytes).expect("decode failed in test");
        assert_eq!(decoded, t, "BitDyn must SSZ-roundtrip correctly.");
        Ok(())
    }

    #[test]
    fn bitdyn_ssz_round_trip() -> Result<(), Error> {
        // length = 8, set even bits
        let mut b = BitVectorDynamic::new(8)?;
        for j in 0..8 {
            if j % 2 == 0 {
                b.set(j, true)?;
            }
        }
        assert_round_trip_bitdyn(b)?;

        // length = 16, all bits
        let mut b = BitVectorDynamic::new(16)?;
        for j in 0..16 {
            b.set(j, true)?;
        }
        assert_round_trip_bitdyn(b)?;

        Ok(())
    }

    #[test]
    fn test_ssz_roundtrip_various_sizes() -> Result<(), Error> {
        // Test empty vector (8 bits)
        let empty = BitVectorDynamic::new(8)?;
        assert_round_trip_bitdyn(empty)?;

        // Test partially filled vector (16 bits)
        let mut partial = BitVectorDynamic::new(16)?;
        partial.set(0, true)?;
        partial.set(8, true)?;
        partial.set(15, true)?;
        assert_round_trip_bitdyn(partial)?;

        // Test fully filled vector (24 bits)
        let mut full = BitVectorDynamic::new(24)?;
        for i in 0..24 {
            full.set(i, true)?;
        }
        assert_round_trip_bitdyn(full)?;

        // Test alternating pattern (32 bits)
        let mut alternating = BitVectorDynamic::new(32)?;
        for i in 0..32 {
            alternating.set(i, i % 2 == 0)?;
        }
        assert_round_trip_bitdyn(alternating)?;

        // Test fully filled vector (128 bits)
        let mut full = BitVectorDynamic::new(128)?;
        for i in 0..128 {
            full.set(i, true)?;
        }
        assert_round_trip_bitdyn(full)?;

        Ok(())
    }

    #[test]
    fn test_serde_roundtrip() -> Result<(), Error> {
        use serde_json::{
            de::Deserializer as json_deserializer, ser::Serializer as json_serializer,
        };

        // Create a test BitVectorDynamic
        let mut b = BitVectorDynamic::new(16)?;
        b.set(0, true)?;
        b.set(7, true)?;

        // Create a string buffer and serializer
        let mut output = Vec::new();
        let mut serializer = json_serializer::new(&mut output);

        // Call serialize
        b.serialize(&mut serializer).expect("Serialization failed");

        // Create a deserializer
        let json = String::from_utf8(output).expect("UTF-8 error");
        let mut deserializer = json_deserializer::from_str(&json);

        // Call deserialize
        let deserialized =
            BitVectorDynamic::deserialize(&mut deserializer).expect("Deserialization failed");

        assert_eq!(b, deserialized);

        Ok(())
    }
}
