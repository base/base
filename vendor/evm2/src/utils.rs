//! Shared utility helpers.

use alloc::{borrow::Cow, vec};
use core::hint::cold_path;

use alloy_primitives::{Address, B256, Bytes, b256};

use crate::interpreter::{InstrStop, Result, Word};

/// Returns the number of EVM words needed for `len` bytes.
#[inline]
pub const fn num_words(len: usize) -> usize {
    len.div_ceil(32)
}

/// Converts an address to an EVM word.
#[inline]
pub fn address_to_word(address: &Address) -> Word {
    Word::from_be_slice(address.as_slice())
}

/// Converts a 256-bit hash to an EVM word.
#[inline]
pub const fn b256_to_word(value: B256) -> Word {
    Word::from_be_bytes(value.0)
}

/// Converts an EVM word to an address.
#[inline]
pub fn word_to_address(value: Word) -> Address {
    Address::from_word(B256::from(value.to_be_bytes::<32>()))
}

/// Converts an EVM word to `usize`, returning an invalid-operand OOG stop on overflow.
#[inline]
pub fn word_to_usize(value: Word) -> Result<usize> {
    value.try_into().map_err(|_| {
        cold_path();
        InstrStop::InvalidOperandOOG
    })
}

/// Converts an EVM word to `usize`, saturating on overflow.
#[inline]
pub fn word_to_usize_saturated(value: Word) -> usize {
    value.try_into().unwrap_or(usize::MAX)
}

/// Calculate the linear cost of a precompile.
#[inline]
pub(crate) const fn calc_linear_cost(len: usize, base: u64, word: u64) -> u64 {
    (len as u64).div_ceil(32) * word + base
}

/// Const function for making an address by concatenating the bytes from two given numbers.
///
/// Note that 32 + 128 = 160 = 20 bytes (the length of an address).
///
/// This function is used as a convenience for specifying the addresses of the various precompiles.
#[inline]
pub(crate) const fn u64_to_address(x: u64) -> Address {
    let x = x.to_be_bytes();
    Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7],
    ])
}

/// Right-pads the given slice at `offset` with zeroes until `LEN`.
///
/// Returns the first `LEN` bytes if it does not need padding.
#[inline]
pub(crate) fn right_pad_with_offset<const LEN: usize>(
    data: &[u8],
    offset: usize,
) -> Cow<'_, [u8; LEN]> {
    right_pad(data.get(offset..).unwrap_or_default())
}

/// Right-pads the given slice at `offset` with zeroes until `len`.
///
/// Returns the first `len` bytes if it does not need padding.
#[inline]
#[allow(dead_code)]
pub(crate) fn right_pad_with_offset_vec(data: &[u8], offset: usize, len: usize) -> Cow<'_, [u8]> {
    right_pad_vec(data.get(offset..).unwrap_or_default(), len)
}

/// Right-pads the given slice with zeroes until `LEN`.
///
/// Returns the first `LEN` bytes if it does not need padding.
#[inline]
pub(crate) fn right_pad<const LEN: usize>(data: &[u8]) -> Cow<'_, [u8; LEN]> {
    if let Some(data) = data.get(..LEN) {
        Cow::Borrowed(data.try_into().unwrap())
    } else {
        let mut padded = [0; LEN];
        padded[..data.len()].copy_from_slice(data);
        Cow::Owned(padded)
    }
}

/// Right-pads the given slice with zeroes until `len`.
///
/// Returns the first `len` bytes if it does not need padding.
#[inline]
pub(crate) fn right_pad_vec(data: &[u8], len: usize) -> Cow<'_, [u8]> {
    if let Some(data) = data.get(..len) {
        Cow::Borrowed(data)
    } else {
        let mut padded = vec![0; len];
        padded[..data.len()].copy_from_slice(data);
        Cow::Owned(padded)
    }
}

/// Left-pads the given slice with zeroes until `LEN`.
///
/// Returns the first `LEN` bytes if it does not need padding.
#[inline]
pub(crate) fn left_pad<const LEN: usize>(data: &[u8]) -> Cow<'_, [u8; LEN]> {
    if let Some(data) = data.get(..LEN) {
        Cow::Borrowed(data.try_into().unwrap())
    } else {
        let mut padded = [0; LEN];
        padded[LEN - data.len()..].copy_from_slice(data);
        Cow::Owned(padded)
    }
}

/// Left-pads the given slice with zeroes until `len`.
///
/// Returns the first `len` bytes if it does not need padding.
#[inline]
#[allow(dead_code)]
pub(crate) fn left_pad_vec(data: &[u8], len: usize) -> Cow<'_, [u8]> {
    if let Some(data) = data.get(..len) {
        Cow::Borrowed(data)
    } else {
        let mut padded = vec![0; len];
        padded[len - data.len()..].copy_from_slice(data);
        Cow::Owned(padded)
    }
}

/// Left-pads the given big-endian slice with zeroes until `len`.
///
/// Unlike [`left_pad_vec`], when `data` is longer than `len` this correctly
/// truncates leading (most-significant) bytes instead of trailing ones.
#[inline]
pub(crate) fn left_pad_vec_be(data: &[u8], len: usize) -> Cow<'_, [u8]> {
    if data.len() < len {
        let mut padded = vec![0; len];
        padded[len - data.len()..].copy_from_slice(data);
        Cow::Owned(padded)
    } else {
        Cow::Borrowed(&data[data.len() - len..])
    }
}

/// Converts a boolean to a left-padded 32-byte [`Bytes`] value.
///
/// This is optimized to not allocate at runtime by using 2 static arrays.
#[inline]
pub(crate) const fn bool_to_bytes32(value: bool) -> Bytes {
    Bytes::from_static(&bool_to_b256(value).0)
}

/// Converts a boolean to a left-padded [`B256`] value.
///
/// This is optimized to not allocate at runtime by using 2 static arrays.
#[inline]
pub(crate) const fn bool_to_b256(value: bool) -> &'static B256 {
    const TRUE: &B256 =
        &b256!("0x0000000000000000000000000000000000000000000000000000000000000001");
    const FALSE: &B256 =
        &b256!("0x0000000000000000000000000000000000000000000000000000000000000000");
    if value { TRUE } else { FALSE }
}

#[cfg(test)]
mod tests {
    use core::assert_matches;

    use super::*;

    #[test]
    fn get_with_right_padding() {
        let data = [1, 2, 3, 4];
        let padded = right_pad_with_offset::<8>(&data, 4);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [0, 0, 0, 0, 0, 0, 0, 0]);
        let padded = right_pad_with_offset_vec(&data, 4, 8);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [0, 0, 0, 0, 0, 0, 0, 0]);

        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let padded = right_pad_with_offset::<8>(&data, 0);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 5, 6, 7, 8]);
        let padded = right_pad_with_offset_vec(&data, 0, 8);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 5, 6, 7, 8]);

        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let padded = right_pad_with_offset::<8>(&data, 4);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [5, 6, 7, 8, 0, 0, 0, 0]);
        let padded = right_pad_with_offset_vec(&data, 4, 8);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [5, 6, 7, 8, 0, 0, 0, 0]);
    }

    #[test]
    fn right_padding() {
        let data = [1, 2, 3, 4];
        let padded = right_pad::<8>(&data);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 0, 0, 0, 0]);
        let padded = right_pad_vec(&data, 8);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 0, 0, 0, 0]);

        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let padded = right_pad::<8>(&data);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 5, 6, 7, 8]);
        let padded = right_pad_vec(&data, 8);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn left_padding() {
        let data = [1, 2, 3, 4];
        let padded = left_pad::<8>(&data);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [0, 0, 0, 0, 1, 2, 3, 4]);
        let padded = left_pad_vec(&data, 8);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [0, 0, 0, 0, 1, 2, 3, 4]);

        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let padded = left_pad::<8>(&data);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 5, 6, 7, 8]);
        let padded = left_pad_vec(&data, 8);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn left_padding_be() {
        let data = [1, 2, 3, 4];
        let padded = left_pad_vec_be(&data, 8);
        assert_matches!(padded, Cow::Owned(_));
        assert_eq!(padded[..], [0, 0, 0, 0, 1, 2, 3, 4]);

        let data = [0, 0, 1, 2, 3, 4];
        let padded = left_pad_vec_be(&data, 4);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4]);

        let data = [1, 2, 3, 4];
        let padded = left_pad_vec_be(&data, 4);
        assert_matches!(padded, Cow::Borrowed(_));
        assert_eq!(padded[..], [1, 2, 3, 4]);
    }

    #[test]
    fn bool2bytes() {
        let f = bool_to_bytes32(false);
        assert_eq!(f[..], [0; 32]);
        let t = bool_to_bytes32(true);
        assert_eq!(t.len(), 32);
        assert_eq!(t[..31], [0; 31]);
        assert_eq!(t[31], 1);
    }
}
