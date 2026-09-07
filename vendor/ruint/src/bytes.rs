#[cfg(feature = "alloc")]
#[allow(unused_imports)]
use alloc::{borrow::Cow, vec::Vec};
use core::slice;

use crate::Uint;

// OPT: *_to_smallvec to avoid allocation.
impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// The size of this integer type in bytes. Note that some bits may be
    /// forced zero if BITS is not cleanly divisible by eight.
    pub const BYTES: usize = BITS.div_ceil(8);

    /// Access the underlying store as a little-endian slice of bytes.
    ///
    /// Only available on little-endian targets.
    ///
    /// If `BITS` does not evenly divide 8, it is padded with zero bits in the
    /// most significant position.
    #[cfg(target_endian = "little")]
    #[must_use]
    #[inline(always)]
    pub const fn as_le_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.limbs.as_ptr().cast(), Self::BYTES) }
    }

    /// Access the underlying store as a mutable little-endian slice of bytes.
    ///
    /// Only available on litte-endian targets.
    ///
    /// # Safety
    ///
    /// If `BITS` does not evenly divide 8, it is padded with zero bits in the
    /// most significant position. Setting those bits puts the [`Uint`] in an
    /// invalid state.
    #[cfg(target_endian = "little")]
    #[must_use]
    #[inline(always)]
    pub const unsafe fn as_le_slice_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.limbs.as_mut_ptr().cast(), Self::BYTES) }
    }

    /// Access the underlying store as a little-endian bytes.
    ///
    /// Uses an optimized implementation on little-endian targets.
    #[cfg(feature = "alloc")]
    #[must_use]
    #[inline]
    #[cfg_attr(target_endian = "little", allow(clippy::missing_const_for_fn))] // Not const in big-endian.
    pub fn as_le_bytes(&self) -> Cow<'_, [u8]> {
        // On little endian platforms this is a no-op.
        #[cfg(target_endian = "little")]
        return Cow::Borrowed(self.as_le_slice());

        // In others, reverse each limb and return a copy.
        #[cfg(target_endian = "big")]
        return Cow::Owned({
            let mut limbs = self.limbs;
            for limb in &mut limbs {
                *limb = limb.swap_bytes();
            }
            unsafe { slice::from_raw_parts(limbs.as_ptr().cast(), Self::BYTES).to_vec() }
        });
    }

    /// Access the underlying store as a little-endian bytes with trailing zeros
    /// removed.
    ///
    /// Uses an optimized implementation on little-endian targets.
    #[cfg(feature = "alloc")]
    #[must_use]
    #[inline]
    pub fn as_le_bytes_trimmed(&self) -> Cow<'_, [u8]> {
        match self.as_le_bytes() {
            Cow::Borrowed(slice) => Cow::Borrowed(crate::utils::trim_end_slice(slice, &0)),
            Cow::Owned(mut vec) => {
                crate::utils::trim_end_vec(&mut vec, &0);
                Cow::Owned(vec)
            }
        }
    }

    /// Converts the [`Uint`] to a little-endian byte array of size exactly
    /// [`Self::BYTES`].
    ///
    /// # Panics
    ///
    /// Panics if the generic parameter `BYTES` is not exactly [`Self::BYTES`].
    /// Ideally this would be a compile time error, but this is blocked by
    /// Rust issue [#60551].
    ///
    /// [#60551]: https://github.com/rust-lang/rust/issues/60551
    #[inline]
    #[must_use]
    pub const fn to_le_bytes<const BYTES: usize>(&self) -> [u8; BYTES] {
        const { Self::assert_bytes(BYTES) }

        // Specialized impl
        #[cfg(target_endian = "little")]
        // SAFETY: BYTES == Self::BYTES == self.as_le_slice().len()
        return unsafe { *self.as_le_slice().as_ptr().cast() };

        // Generic impl
        #[cfg(target_endian = "big")]
        {
            let mut limbs = self.limbs;
            const_range_for!(limbs in mut limbs => {
                *limbs = (*limbs).to_le();
            });
            // SAFETY: BYTES <= LIMBS * 8
            unsafe { *limbs.as_ptr().cast() }
        }
    }

    /// Converts the [`Uint`] to a little-endian byte vector of size exactly
    /// [`Self::BYTES`].
    ///
    /// This method is useful when [`Self::to_le_bytes`] can not be used because
    /// byte size is not known compile time.
    #[cfg(feature = "alloc")]
    #[must_use]
    #[inline]
    pub fn to_le_bytes_vec(&self) -> Vec<u8> {
        self.as_le_bytes().into_owned()
    }

    /// Converts the [`Uint`] to a little-endian byte vector with trailing zeros
    /// bytes removed.
    #[cfg(feature = "alloc")]
    #[must_use]
    #[inline]
    pub fn to_le_bytes_trimmed_vec(&self) -> Vec<u8> {
        self.as_le_bytes_trimmed().into_owned()
    }

    /// Converts the [`Uint`] to a big-endian byte array of size exactly
    /// [`Self::BYTES`].
    ///
    /// # Panics
    ///
    /// Panics if the generic parameter `BYTES` is not exactly [`Self::BYTES`].
    /// Ideally this would be a compile time error, but this is blocked by
    /// Rust issue [#60551].
    ///
    /// [#60551]: https://github.com/rust-lang/rust/issues/60551
    #[must_use]
    #[inline]
    pub const fn to_be_bytes<const BYTES: usize>(&self) -> [u8; BYTES] {
        const { Self::assert_bytes(BYTES) }

        // Optimized implementation for full-limb types: one `bswap` per limb.
        // The byte-by-byte reverse in the generic fallback is not recognized
        // by LLVM and degrades to 32 individual byte loads/stores for U256.
        if BYTES == LIMBS * 8 {
            let mut limbs_be = [0u64; LIMBS];
            const_range_for!(i in 0..LIMBS => {
                limbs_be[i] = self.limbs[LIMBS - 1 - i].to_be();
            });
            // SAFETY: BYTES == size_of::<[u64; LIMBS]>().
            return unsafe { *limbs_be.as_ptr().cast() };
        }

        let mut bytes = self.to_le_bytes::<BYTES>();
        bytes.reverse();
        bytes
    }

    /// Converts the [`Uint`] to a big-endian byte vector of size exactly
    /// [`Self::BYTES`].
    ///
    /// This method is useful when [`Self::to_be_bytes`] can not be used because
    /// byte size is not known compile time.
    #[cfg(feature = "alloc")]
    #[must_use]
    #[inline]
    pub fn to_be_bytes_vec(&self) -> Vec<u8> {
        let mut bytes = self.to_le_bytes_vec();
        bytes.reverse();
        bytes
    }

    /// Converts the [`Uint`] to a big-endian byte vector with leading zeros
    /// bytes removed.
    #[cfg(feature = "alloc")]
    #[must_use]
    #[inline]
    pub fn to_be_bytes_trimmed_vec(&self) -> Vec<u8> {
        let mut bytes = self.to_le_bytes_trimmed_vec();
        bytes.reverse();
        bytes
    }

    /// Converts a big-endian byte array of size exactly
    /// [`Self::BYTES`] to [`Uint`].
    ///
    /// # Panics
    ///
    /// Panics if the generic parameter `BYTES` is not exactly [`Self::BYTES`].
    /// Ideally this would be a compile time error, but this is blocked by
    /// Rust issue [#60551].
    ///
    /// [#60551]: https://github.com/rust-lang/rust/issues/60551
    ///
    /// Panics if the value is too large for the bit-size of the Uint.
    #[must_use]
    #[track_caller]
    #[inline]
    pub const fn from_be_bytes<const BYTES: usize>(bytes: [u8; BYTES]) -> Self {
        const { Self::assert_bytes(BYTES) }
        Self::from_be_slice(&bytes)
    }

    /// Creates a new integer from a big endian slice of bytes.
    ///
    /// The slice is interpreted as a big endian number, and must be at most
    /// [`Self::BYTES`] long.
    ///
    /// # Panics
    ///
    /// Panics if the value is larger than fits the [`Uint`].
    #[must_use]
    #[track_caller]
    #[inline]
    pub const fn from_be_slice(bytes: &[u8]) -> Self {
        match Self::try_from_be_slice(bytes) {
            Some(value) => value,
            None => panic!("Value too large for Uint"),
        }
    }

    /// Creates a new integer from a big endian slice of bytes.
    ///
    /// The slice is interpreted as a big endian number, and must be at most
    /// [`Self::BYTES`] long.
    ///
    /// Returns [`None`] if the value is larger than fits the [`Uint`].
    #[must_use]
    #[inline]
    pub const fn try_from_be_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > Self::BYTES {
            return None;
        }

        if Self::BYTES % 8 == 0 && bytes.len() == Self::BYTES {
            // Optimized implementation for full-limb types.
            let mut limbs = [0; LIMBS];
            let end = bytes.as_ptr_range().end;
            const_range_for!(i in 0..LIMBS => {
                limbs[i] = u64::from_be_bytes(unsafe { *end.sub((i + 1) * 8).cast() });
            });
            return Some(Self::from_limbs(limbs));
        }

        let mut limbs = [0; LIMBS];
        let mut c = bytes.len();
        const_range_for!(i in 0..bytes.len() => {
            c -= 1;
            let (limb, byte) = (i / 8, i % 8);
            limbs[limb] += (bytes[c] as u64) << (byte * 8);
        });
        if LIMBS > 0 && limbs[LIMBS - 1] > Self::MASK {
            return None;
        }
        Some(Self::from_limbs(limbs))
    }

    /// Converts a little-endian byte array of size exactly
    /// [`Self::BYTES`] to [`Uint`].
    ///
    /// # Panics
    ///
    /// Panics if the generic parameter `BYTES` is not exactly [`Self::BYTES`].
    /// Ideally this would be a compile time error, but this is blocked by
    /// Rust issue [#60551].
    ///
    /// [#60551]: https://github.com/rust-lang/rust/issues/60551
    ///
    /// Panics if the value is too large for the bit-size of the Uint.
    #[must_use]
    #[track_caller]
    #[inline]
    pub const fn from_le_bytes<const BYTES: usize>(bytes: [u8; BYTES]) -> Self {
        const { Self::assert_bytes(BYTES) }
        Self::from_le_slice(&bytes)
    }

    /// Creates a new integer from a little endian slice of bytes.
    ///
    /// The slice is interpreted as a little endian number, and must be at most
    /// [`Self::BYTES`] long.
    ///
    /// # Panics
    ///
    /// Panics if the value is larger than fits the [`Uint`].
    #[must_use]
    #[track_caller]
    #[inline]
    pub const fn from_le_slice(bytes: &[u8]) -> Self {
        match Self::try_from_le_slice(bytes) {
            Some(value) => value,
            None => panic!("Value too large for Uint"),
        }
    }

    /// Creates a new integer from a little endian slice of bytes.
    ///
    /// The slice is interpreted as a little endian number, and must be at most
    /// [`Self::BYTES`] long.
    ///
    /// Returns [`None`] if the value is larger than fits the [`Uint`].
    #[must_use]
    #[inline]
    pub const fn try_from_le_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > Self::BYTES {
            return None;
        }

        if Self::BYTES % 8 == 0 && bytes.len() == Self::BYTES {
            // Optimized implementation for full-limb types.
            let mut limbs = [0; LIMBS];
            const_range_for!(i in 0..LIMBS => {
                limbs[i] = u64::from_le_bytes(unsafe { *bytes.as_ptr().add(i * 8).cast() });
            });
            return Some(Self::from_limbs(limbs));
        }

        let mut limbs = [0; LIMBS];
        const_range_for!(i in 0..bytes.len() => {
            let (limb, byte) = (i / 8, i % 8);
            limbs[limb] += (bytes[i] as u64) << (byte * 8);
        });
        if LIMBS > 0 && limbs[LIMBS - 1] > Self::MASK {
            return None;
        }
        Some(Self::from_limbs(limbs))
    }

    /// Writes the little-endian representation of the [`Uint`] to the given
    /// buffer. The buffer must be large enough to hold [`Self::BYTES`] bytes.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is not large enough to hold [`Self::BYTES`] bytes.
    ///
    /// # Returns
    ///
    /// The number of bytes written to the buffer (always equal to
    /// [`Self::BYTES`], but often useful to make explicit for encoders).
    #[inline]
    pub fn copy_le_bytes_to(&self, buf: &mut [u8]) -> usize {
        // This is debug only. Release panics occur later in copy_from_slice
        debug_assert!(
            buf.len() >= Self::BYTES,
            "Buffer is too small to hold the bytes of the Uint"
        );

        #[cfg(target_endian = "little")]
        buf[..Self::BYTES].copy_from_slice(self.as_le_slice());

        #[cfg(target_endian = "big")]
        {
            let chunks = buf[..Self::BYTES].chunks_mut(8);

            self.limbs.iter().zip(chunks).for_each(|(&limb, chunk)| {
                let le = limb.to_le_bytes();
                chunk.copy_from_slice(&le[..chunk.len()]);
            });
        }

        Self::BYTES
    }

    /// Writes the little-endian representation of the [`Uint`] to the given
    /// buffer. The buffer must be large enough to hold [`Self::BYTES`] bytes.
    ///
    /// # Returns
    ///
    /// [`None`], if the buffer is not large enough to hold [`Self::BYTES`]
    /// bytes, and does not modify the buffer.
    ///
    /// [`Some`] with the number of bytes written to the buffer (always
    /// equal to [`Self::BYTES`], but often useful to make explicit for
    /// encoders).
    #[inline]
    pub fn checked_copy_le_bytes_to(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < Self::BYTES {
            return None;
        }

        Some(self.copy_le_bytes_to(buf))
    }

    /// Writes the big-endian representation of the [`Uint`] to the given
    /// buffer. The buffer must be large enough to hold [`Self::BYTES`] bytes.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is not large enough to hold [`Self::BYTES`] bytes.
    ///
    /// # Returns
    ///
    /// The number of bytes written to the buffer (always equal to
    /// [`Self::BYTES`], but often useful to make explicit for encoders).
    #[inline]
    pub fn copy_be_bytes_to(&self, buf: &mut [u8]) -> usize {
        // This is debug only. Release panics occur later in copy_from_slice
        debug_assert!(
            buf.len() >= Self::BYTES,
            "Buffer is too small to hold the bytes of the Uint"
        );

        // start from the end of the slice
        let chunks = buf[..Self::BYTES].rchunks_mut(8);

        self.limbs.iter().zip(chunks).for_each(|(&limb, chunk)| {
            let be = limb.to_be_bytes();
            let copy_from = 8 - chunk.len();
            chunk.copy_from_slice(&be[copy_from..]);
        });

        Self::BYTES
    }

    /// Writes the big-endian representation of the [`Uint`] to the given
    /// buffer. The buffer must be large enough to hold [`Self::BYTES`] bytes.
    ///
    /// # Returns
    ///
    /// [`None`], if the buffer is not large enough to hold [`Self::BYTES`]
    /// bytes, and does not modify the buffer.
    ///
    /// [`Some`] with the number of bytes written to the buffer (always
    /// equal to [`Self::BYTES`], but often useful to make explicit for
    /// encoders).
    #[inline]
    pub fn checked_copy_be_bytes_to(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < Self::BYTES {
            return None;
        }

        Some(self.copy_be_bytes_to(buf))
    }

    #[track_caller]
    const fn assert_bytes(bytes: usize) {
        assert!(bytes == Self::BYTES, "BYTES must be equal to Self::BYTES");
    }
}

/// Number of bytes required to represent the given number of bits.
///
/// This needs to be public because it is used in the `Uint` type,
/// specifically in the [`to_be_bytes()`][Uint::to_be_bytes] and related
/// functions.
#[inline]
#[must_use]
pub const fn nbytes(bits: usize) -> usize {
    bits.div_ceil(8)
}

#[cfg(test)]
mod tests {
    use proptest::proptest;

    use super::*;
    use crate::{const_for, nlimbs};

    const N: Uint<128, 2> =
        Uint::from_limbs([0x7890_1234_5678_9012_u64, 0x1234_5678_9012_3456_u64]);
    const BE: [u8; 16] = [
        0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90,
        0x12,
    ];
    const LE: [u8; 16] = [
        0x12, 0x90, 0x78, 0x56, 0x34, 0x12, 0x90, 0x78, 0x56, 0x34, 0x12, 0x90, 0x78, 0x56, 0x34,
        0x12,
    ];

    const K: Uint<72, 2> = Uint::from_limbs([0x3456_7890_1234_5678_u64, 0x12_u64]);
    const KBE: [u8; 9] = [0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78];
    const KLE: [u8; 9] = [0x78, 0x56, 0x34, 0x12, 0x90, 0x78, 0x56, 0x34, 0x12];

    #[test]
    const fn const_from_to_bytes() {
        const NL: [u64; 2] = N.limbs;
        const KL: [u64; 2] = K.limbs;
        assert!(matches!(Uint::<128, 2>::from_be_bytes(BE).limbs, NL));
        assert!(matches!(Uint::<128, 2>::from_le_bytes(LE).limbs, NL));
        assert!(matches!(N.to_be_bytes::<{ BE.len() }>(), BE));
        assert!(matches!(N.to_le_bytes::<{ LE.len() }>(), LE));

        assert!(matches!(Uint::<72, 2>::from_be_bytes(KBE).limbs, KL));
        assert!(matches!(Uint::<72, 2>::from_le_bytes(KLE).limbs, KL));
        assert!(matches!(K.to_be_bytes::<{ KBE.len() }>(), KBE));
        assert!(matches!(K.to_le_bytes::<{ KLE.len() }>(), KLE));

        assert!(matches!(Uint::<0, 0>::ZERO.to_be_bytes::<0>(), []));
        assert!(matches!(Uint::<1, 1>::ZERO.to_be_bytes::<1>(), [0]));
        assert!(matches!(Uint::<1, 1>::from_limbs([1]).to_be_bytes::<1>(), [1]));
        assert!(matches!(Uint::<16, 1>::from_limbs([0x1234]).to_be_bytes::<2>(), [0x12, 0x34]));

        assert!(matches!(Uint::<0, 0>::ZERO.to_be_bytes::<0>(), []));
        assert!(matches!(Uint::<0, 0>::ZERO.to_le_bytes::<0>(), []));
        assert!(matches!(Uint::<1, 1>::ZERO.to_be_bytes::<1>(), [0]));
        assert!(matches!(Uint::<1, 1>::ZERO.to_le_bytes::<1>(), [0]));
        assert!(matches!(Uint::<1, 1>::from_limbs([1]).to_be_bytes::<1>(), [1]));
        assert!(matches!(Uint::<1, 1>::from_limbs([1]).to_le_bytes::<1>(), [1]));
        assert!(matches!(Uint::<16, 1>::from_limbs([0x1234]).to_be_bytes::<2>(), [0x12, 0x34]));
        assert!(matches!(Uint::<16, 1>::from_limbs([0x1234]).to_le_bytes::<2>(), [0x34, 0x12]));

        assert!(matches!(
            Uint::<63, 1>::from_limbs([0x010203]).to_be_bytes::<8>(),
            [0, 0, 0, 0, 0, 1, 2, 3]
        ));
        assert!(matches!(
            Uint::<63, 1>::from_limbs([0x010203]).to_le_bytes::<8>(),
            [3, 2, 1, 0, 0, 0, 0, 0]
        ));
    }

    #[test]
    fn test_from_bytes() {
        assert_eq!(Uint::<0, 0>::from_be_bytes([]), Uint::ZERO);
        assert_eq!(Uint::<0, 0>::from_le_bytes([]), Uint::ZERO);
        assert_eq!(Uint::<12, 1>::from_be_bytes([0x01, 0x23]), Uint::from(0x0123));
        assert_eq!(Uint::<12, 1>::from_le_bytes([0x23, 0x01]), Uint::from(0x0123));
        assert_eq!(Uint::<16, 1>::from_be_bytes([0x12, 0x34]), Uint::from(0x1234));
        assert_eq!(Uint::<16, 1>::from_le_bytes([0x34, 0x12]), Uint::from(0x1234));

        assert_eq!(Uint::from_be_bytes(BE), N);
        assert_eq!(Uint::from_le_bytes(LE), N);
        assert_eq!(Uint::from_be_bytes(KBE), K);
        assert_eq!(Uint::from_le_bytes(KLE), K);

        assert_eq!(Uint::<128, 2>::try_from_be_slice(&BE), Some(N));
        assert_eq!(Uint::<128, 2>::try_from_be_slice(&[&BE[..], &[0xff][..]].concat()), None);
        assert_eq!(Uint::<128, 2>::try_from_le_slice(&LE), Some(N));
        assert_eq!(Uint::<128, 2>::try_from_le_slice(&[&LE[..], &[0xff]].concat()), None);
        assert_eq!(Uint::<72, 2>::try_from_be_slice(&KBE), Some(K));
        assert_eq!(Uint::<72, 2>::try_from_be_slice(&[&KBE[..], &[0xff][..]].concat()), None);
        assert_eq!(Uint::<72, 2>::try_from_le_slice(&KLE), Some(K));
        assert_eq!(Uint::<72, 2>::try_from_le_slice(&[&KLE[..], &[0xff]].concat()), None);
    }

    #[test]
    fn test_to_bytes() {
        assert_eq!(Uint::<0, 0>::ZERO.to_le_bytes(), [0_u8; 0]);
        assert_eq!(Uint::<0, 0>::ZERO.to_be_bytes(), [0_u8; 0]);
        assert_eq!(Uint::<12, 1>::from(0x0123_u64).to_le_bytes(), [0x23, 0x01]);
        assert_eq!(Uint::<12, 1>::from(0x0123_u64).to_be_bytes(), [0x01, 0x23]);
        assert_eq!(Uint::<16, 1>::from(0x1234_u64).to_le_bytes(), [0x34, 0x12]);
        assert_eq!(Uint::<16, 1>::from(0x1234_u64).to_be_bytes(), [0x12, 0x34]);
        assert_eq!(K.to_be_bytes(), KBE);
        assert_eq!(K.to_le_bytes(), KLE);
    }

    #[test]
    fn test_bytes_roundtrip() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            const BYTES: usize = nbytes(BITS);
            proptest!(|(value: Uint<BITS, LIMBS>)| {
                assert_eq!(value, Uint::try_from_le_slice(&value.as_le_bytes()).unwrap());
                assert_eq!(value, Uint::try_from_le_slice(&value.as_le_bytes_trimmed()).unwrap());
                assert_eq!(value, Uint::try_from_be_slice(&value.to_be_bytes_trimmed_vec()).unwrap());
                assert_eq!(value, Uint::try_from_le_slice(&value.to_le_bytes_trimmed_vec()).unwrap());
                assert_eq!(value, Uint::from_be_bytes(value.to_be_bytes::<BYTES>()));
                assert_eq!(value, Uint::from_le_bytes(value.to_le_bytes::<BYTES>()));
            });
        });
    }

    #[test]
    fn copy_to() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            const BYTES: usize = nbytes(BITS);
            proptest!(|(value: Uint<BITS, LIMBS>)|{
                let mut buf = [0; BYTES];
                value.copy_le_bytes_to(&mut buf);
                assert_eq!(buf, value.to_le_bytes::<BYTES>());
                assert_eq!(value, Uint::try_from_le_slice(&buf).unwrap());

                let mut buf = [0; BYTES];
                value.copy_be_bytes_to(&mut buf);
                assert_eq!(buf, value.to_be_bytes::<BYTES>());
                assert_eq!(value, Uint::try_from_be_slice(&buf).unwrap());
            });
        });
    }

    #[test]
    fn checked_copy_to() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            const BYTES: usize = nbytes(BITS);
            proptest!(|(value: Uint<BITS, LIMBS>)|{
                if BYTES != 0 {
                    let mut buf = [0; BYTES];
                    let too_short = buf.len() - 1;

                    assert_eq!(value.checked_copy_le_bytes_to(&mut buf[..too_short]), None);
                    assert_eq!(buf, [0; BYTES], "buffer was modified");

                    assert_eq!(value.checked_copy_be_bytes_to(&mut buf[..too_short]), None);
                    assert_eq!(buf, [0; BYTES], "buffer was modified");
                }
            });
        });
    }
}
