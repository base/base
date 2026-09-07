#![cfg(feature = "ssz")]
#![cfg_attr(docsrs, doc(cfg(feature = "ssz")))]
use ethereum_ssz_0_5_3::{Decode, DecodeError, Encode};

use crate::{Uint, nbytes};

impl<const BITS: usize, const LIMBS: usize> Encode for Uint<BITS, LIMBS> {
    fn is_ssz_fixed_len() -> bool {
        true
    }

    fn ssz_fixed_len() -> usize {
        nbytes(BITS)
    }

    fn ssz_bytes_len(&self) -> usize {
        nbytes(BITS)
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.as_le_bytes());
    }
}

impl<const BITS: usize, const LIMBS: usize> Decode for Uint<BITS, LIMBS> {
    fn is_ssz_fixed_len() -> bool {
        true
    }

    fn ssz_fixed_len() -> usize {
        nbytes(BITS)
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() > nbytes(BITS) {
            return Err(DecodeError::InvalidByteLength {
                len: bytes.len(),
                expected: nbytes(BITS),
            });
        }
        Ok(Self::from_le_slice(bytes))
    }
}

#[cfg(test)]
mod tests {
    use ethereum_ssz_0_5_3::DecodeError;
    use proptest::proptest;
    use ruint::{Uint, const_for, nlimbs};

    #[test]
    fn test_ssz_human_readable() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            proptest!(|(value: Uint<BITS, LIMBS>)| {
                let expected = value;
                let encoded = ethereum_ssz_0_5_3::Encode::as_ssz_bytes(&expected);
                let actual: Uint<BITS, LIMBS> = ethereum_ssz_0_5_3::Decode::from_ssz_bytes(&encoded).unwrap();
                assert_eq!(expected, actual, "Failed for value: {value:?}" );
            });

        });
    }

    #[test]
    fn test_ssz_decode_error_length() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            proptest!(|(value: Uint<BITS, LIMBS>)| {
                let encoded = ethereum_ssz_0_5_3::Encode::as_ssz_bytes(&value);
                let mut oversized = encoded;
                oversized.push(0);

                let result = <Uint<BITS, LIMBS> as ethereum_ssz_0_5_3::Decode>::from_ssz_bytes(&oversized);
                assert!(matches!(result, Err(DecodeError::InvalidByteLength { len:_, expected:_ })));
            });
        });
    }
}
