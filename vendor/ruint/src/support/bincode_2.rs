//! Support for the [`bincode`](https://crates.io/crates/bincode) crate.

#![cfg(feature = "bincode-2")]
#![cfg_attr(docsrs, doc(cfg(feature = "bincode-2")))]

use bincode::{
    de::{BorrowDecode, BorrowDecoder, Decode, Decoder, read::Reader},
    enc::{Encode, Encoder},
    error::{DecodeError, EncodeError},
};

use crate::{Bits, Uint};

impl<const BITS: usize, const LIMBS: usize> Encode for Uint<BITS, LIMBS> {
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        if BITS == 0 {
            return Ok(());
        }

        #[cfg(target_endian = "little")]
        return Encode::encode(self.as_le_slice(), encoder);

        #[cfg(target_endian = "big")]
        {
            let mut limbs = self.limbs;
            let mut i = 0;
            while i < LIMBS {
                limbs[i] = limbs[i].to_le();
                i += 1;
            }
            // SAFETY: BYTES <= LIMBS * 8
            let slice: &[u8] = unsafe {
                let ptr = limbs.as_ptr() as *const u8;
                core::slice::from_raw_parts(ptr, Self::BYTES)
            };
            Encode::encode(slice, encoder)
        }
    }
}

impl<Context, const BITS: usize, const LIMBS: usize> Decode<Context> for Uint<BITS, LIMBS> {
    fn decode<D: Decoder<Context = Context>>(decoder: &mut D) -> Result<Self, DecodeError> {
        if BITS == 0 {
            return Ok(Self::ZERO);
        }
        let len = decode_slice_len(decoder)?;
        if len != Self::BYTES {
            return Err(DecodeError::ArrayLengthMismatch { required: Self::BYTES, found: len });
        }

        decoder.claim_bytes_read(len)?;
        let mut buffer = [0u64; LIMBS]; // not possible to use Self::BYTES or nbytes(BITS) here.
        let slice = unsafe {
            // SAFETY: We ensure that the buffer is large enough to hold the bytes
            let ptr = buffer.as_mut_ptr().cast::<u8>();
            core::slice::from_raw_parts_mut(ptr, Self::BYTES)
        };
        decoder.reader().read(slice)?;
        Ok(Self::from_le_slice(&*slice))
    }
}

impl<'de, Context, const BITS: usize, const LIMBS: usize> BorrowDecode<'de, Context>
    for Uint<BITS, LIMBS>
{
    fn borrow_decode<D: BorrowDecoder<'de, Context = Context>>(
        decoder: &mut D,
    ) -> Result<Self, DecodeError> {
        if BITS == 0 {
            return Ok(Self::ZERO);
        }
        let bytes: &'de [u8] = BorrowDecode::borrow_decode(decoder)?;
        if bytes.len() != Self::BYTES {
            return Err(DecodeError::ArrayLengthMismatch {
                required: Self::BYTES,
                found: bytes.len(),
            });
        }
        Ok(Self::from_le_slice(bytes))
    }
}

impl<const BITS: usize, const LIMBS: usize> Encode for Bits<BITS, LIMBS> {
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        self.as_uint().encode(encoder)
    }
}

impl<Context, const BITS: usize, const LIMBS: usize> Decode<Context> for Bits<BITS, LIMBS> {
    fn decode<D: Decoder<Context = Context>>(decoder: &mut D) -> Result<Self, DecodeError> {
        let uint: Uint<BITS, LIMBS> = Decode::decode(decoder)?;
        Ok(Self::from(uint))
    }
}

impl<'de, Context, const BITS: usize, const LIMBS: usize> BorrowDecode<'de, Context>
    for Bits<BITS, LIMBS>
{
    fn borrow_decode<D: BorrowDecoder<'de, Context = Context>>(
        decoder: &mut D,
    ) -> Result<Self, DecodeError> {
        let uint: Uint<BITS, LIMBS> = BorrowDecode::borrow_decode(decoder)?;
        Ok(Self::from(uint))
    }
}

/// Decodes the length of any slice, container, etc from the decoder
#[inline]
fn decode_slice_len<D: Decoder>(decoder: &mut D) -> Result<usize, DecodeError> {
    let v = u64::decode(decoder)?;

    v.try_into().map_err(|_| DecodeError::OutsideUsizeRange(v))
}

#[cfg(test)]
mod tests {
    use bincode::{borrow_decode_from_slice, config::Config, decode_from_slice, encode_into_slice};
    use proptest::proptest;

    use super::*;
    use crate::{const_for, nbytes, nlimbs};

    #[test]
    fn test_bincode_2() {
        test_bincode_2_inner(bincode::config::standard());
        test_bincode_2_inner(bincode::config::legacy());
    }

    fn test_bincode_2_inner<C: Config>(config: C) {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            const BUFFER_SIZE: usize = nbytes(BITS) + 8; // usize length takes at most 8 bytes
            proptest!(|(value: Uint<BITS, LIMBS>)| {
                let mut buffer = [0u8; BUFFER_SIZE];
                let bytes_written = encode_into_slice(value, &mut buffer, config).unwrap();
                let (deserialized, bytes_read) = decode_from_slice::<Uint<BITS, LIMBS>, _>(&buffer, config).unwrap();
                assert_eq!(bytes_read, bytes_written);
                assert_eq!(value, deserialized);
                let (deserialized, bytes_read) = borrow_decode_from_slice::<Uint<BITS, LIMBS>, _>(&buffer, config).unwrap();
                assert_eq!(bytes_read, bytes_written);
                assert_eq!(value, deserialized);
            });
            proptest!(|(value: Bits<BITS, LIMBS>)| {
                let mut buffer = [0u8; BUFFER_SIZE];
                let bytes_written = encode_into_slice(value, &mut buffer, config).unwrap();
                let (deserialized, bytes_read) = decode_from_slice::<Bits<BITS, LIMBS>, _>(&buffer, config).unwrap();
                assert_eq!(bytes_read, bytes_written);
                assert_eq!(value, deserialized);
                let (deserialized, bytes_read) = borrow_decode_from_slice::<Bits<BITS, LIMBS>, _>(&buffer, config).unwrap();
                assert_eq!(bytes_read, bytes_written);
                assert_eq!(value, deserialized);
            });
        });
    }
}
