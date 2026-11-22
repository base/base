//! Module for working with span batch bits.

use alloc::{vec, vec::Vec};
use core::cmp::Ordering;

use alloy_primitives::bytes;
use alloy_rlp::Buf;

use crate::SpanBatchError;

/// Type for span batch bits.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpanBatchBits(pub Vec<u8>);

impl AsRef<[u8]> for SpanBatchBits {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl SpanBatchBits {
    /// Creates a new span batch bits.
    pub const fn new(inner: Vec<u8>) -> Self {
        Self(inner)
    }

    /// Decodes a standard span-batch bitlist from a reader.
    /// The bitlist is encoded as big-endian integer, left-padded with zeroes to a multiple of 8
    /// bits. The encoded bitlist cannot be longer than `bit_length`.
    pub fn decode(b: &mut &[u8], bit_length: usize) -> Result<Self, SpanBatchError> {
        let buffer_len = bit_length / 8 + if !bit_length.is_multiple_of(8) { 1 } else { 0 };
        let bits = if b.len() < buffer_len {
            let mut bits = vec![0; buffer_len];
            bits[..b.len()].copy_from_slice(b);
            b.advance(b.len());
            bits
        } else {
            let v = b[..buffer_len].to_vec();
            b.advance(buffer_len);
            v
        };
        let sb_bits = Self(bits);

        if sb_bits.bit_len() > bit_length {
            return Err(SpanBatchError::BitfieldTooLong);
        }

        Ok(sb_bits)
    }

    /// Encodes a standard span-batch bitlist.
    /// The bitlist is encoded as big-endian integer, left-padded with zeroes to a multiple of 8
    /// bits. The encoded bitlist cannot be longer than `bit_length`
    pub fn encode(
        w: &mut dyn bytes::BufMut,
        bit_length: usize,
        bits: &Self,
    ) -> Result<(), SpanBatchError> {
        if bits.bit_len() > bit_length {
            return Err(SpanBatchError::BitfieldTooLong);
        }

        // Round up, ensure enough bytes when number of bits is not a multiple of 8.
        // Alternative of (L+7)/8 is not overflow-safe.
        let buf_len = bit_length / 8 + if !bit_length.is_multiple_of(8) { 1 } else { 0 };
        let mut buf = vec![0; buf_len];
        buf[buf_len - bits.0.len()..].copy_from_slice(bits.as_ref());
        w.put_slice(&buf);
        Ok(())
    }

    /// Get a bit from the [`SpanBatchBits`] bitlist.
    pub fn get_bit(&self, index: usize) -> Option<u8> {
        let byte_index = index / 8;
        let bit_index = index % 8;

        if byte_index < self.0.len() {
            let byte = self.0[self.0.len() - byte_index - 1];

            Some(if byte & (1 << bit_index) != 0 { 1 } else { 0 })
        } else {
            None
        }
    }

    /// Sets a bit in the [`SpanBatchBits`] bitlist.
    pub fn set_bit(&mut self, index: usize, value: bool) {
        let byte_index = index / 8;
        let bit_index = index % 8;

        if byte_index >= self.0.len() {
            Self::resize_from_right(&mut self.0, byte_index + 1);
        }

        let len = self.0.len();
        let byte = &mut self.0[len - byte_index - 1];

        if value {
            *byte |= 1 << bit_index;
        } else {
            *byte &= !(1 << bit_index);
        }
    }

    /// Calculates the bit length of the [`SpanBatchBits`] bitfield.
    pub fn bit_len(&self) -> usize {
        for (i, &byte) in self.0.iter().enumerate() {
            if byte != 0 {
                let msb_index = 7 - byte.leading_zeros() as usize;

                let total_bit_length = msb_index + 1 + ((self.0.len() - i - 1) * 8);
                return total_bit_length;
            }
        }

        0
    }

    /// Resizes an array from the right. Useful for big-endian zero extension.
    fn resize_from_right<T: Default + Copy>(vec: &mut Vec<T>, new_size: usize) {
        let current_size = vec.len();
        match new_size.cmp(&current_size) {
            Ordering::Less => {
                let remove_count = current_size - new_size;
                vec.drain(0..remove_count);
            }
            Ordering::Greater => {
                let additional = new_size - current_size;
                let mut prepend_elements = vec![T::default(); new_size];
                prepend_elements[additional..].copy_from_slice(vec.as_ref());
                *vec = prepend_elements;
            }
            Ordering::Equal => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::{collection::vec, prelude::any, proptest};
    use rstest::rstest;

    use super::*;

    proptest! {
        #[test]
        fn test_encode_decode_roundtrip_span_bitlist(vec in vec(any::<u8>(), 0..5096)) {
            let bits = SpanBatchBits(vec);
            assert_eq!(SpanBatchBits::decode(&mut bits.as_ref(), bits.0.len() * 8).unwrap(), bits);
            let mut encoded = Vec::new();
            SpanBatchBits::encode(&mut encoded, bits.0.len() * 8, &bits).unwrap();
            assert_eq!(encoded, bits.0);
        }

        #[test]
        fn test_span_bitlist_bitlen(index in 0usize..65536) {
            let mut bits = SpanBatchBits::default();
            bits.set_bit(index, true);
            assert_eq!(bits.0.len(), (index / 8) + 1);
            assert_eq!(bits.bit_len(), index + 1);
        }

        #[test]
        fn test_span_bitlist_bitlen_shrink(first_index in 8usize..65536) {
            let second_index = first_index.clamp(0, first_index - 8);
            let mut bits = SpanBatchBits::default();

            bits.set_bit(first_index, true);
            assert_eq!(bits.0.len(), (first_index / 8) + 1);
            assert_eq!(bits.bit_len(), first_index + 1);
            bits.set_bit(first_index, false);
            assert_eq!(bits.0.len(), (first_index / 8) + 1);
            assert_eq!(bits.bit_len(), 0);

            // Set second bit. Even though the array is larger, as it was originally allocated with more words,
            // the bitlength should still be lowered as the higher-order words are 0'd out.
            bits.set_bit(second_index, true);
            assert_eq!(bits.0.len(), (first_index / 8) + 1);
            assert_eq!(bits.bit_len(), second_index + 1);
        }
    }

    #[test]
    fn bitlist_big_endian_zero_extended() {
        let mut bits = SpanBatchBits::default();

        bits.set_bit(1, true);
        bits.set_bit(6, true);
        bits.set_bit(8, true);
        bits.set_bit(15, true);
        assert_eq!(bits.0[0], 0b1000_0001);
        assert_eq!(bits.0[1], 0b0100_0010);
        assert_eq!(bits.0.len(), 2);
        assert_eq!(bits.bit_len(), 16);
    }

    #[test]
    fn test_static_set_get_bits_span_bitlist() {
        let mut bits = SpanBatchBits::default();
        assert!(bits.0.is_empty());

        bits.set_bit(0, true);
        bits.set_bit(1, true);
        bits.set_bit(2, true);
        bits.set_bit(4, true);
        bits.set_bit(7, true);
        assert_eq!(bits.0.len(), 1);
        assert_eq!(bits.get_bit(0), Some(1));
        assert_eq!(bits.get_bit(1), Some(1));
        assert_eq!(bits.get_bit(2), Some(1));
        assert_eq!(bits.get_bit(3), Some(0));
        assert_eq!(bits.get_bit(4), Some(1));

        bits.set_bit(17, true);
        assert_eq!(bits.get_bit(17), Some(1));
        assert_eq!(bits.get_bit(32), None);
        assert_eq!(bits.0.len(), 3);
    }

    #[rstest]
    #[case::bitlist_bitlen_empty_extend(vec![], 7)]
    #[case::bitlist_bitlen_extend(vec![0b0100_0010, 0b1000_0001], 16)]
    #[case::bitlist_bitlen_same(vec![0b0100_0010, 0b1000_0001, 0b0101_0011], 23)]
    fn test_span_bitlist_resize(#[case] init: Vec<u8>, #[case] idx: usize) {
        let mut bits = SpanBatchBits(init);
        bits.set_bit(idx, true);

        let byte_idx = idx / 8;
        let bit_idx = idx % 8 + 1;
        let expected_len = (byte_idx * 8) + bit_idx;

        assert_eq!(bits.get_bit(idx), Some(1));
        assert_eq!(bits.bit_len(), expected_len);
    }
}
