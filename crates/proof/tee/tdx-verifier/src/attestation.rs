//! TDX signer attestation payload encoding.

use alloy_primitives::{B256, Bytes};
use thiserror::Error;

/// Magic prefix for encoded TDX signer attestations returned by JSON-RPC.
const TDX_SIGNER_ATTESTATION_MAGIC: &[u8; 8] = b"BASETDX3";

/// Encoded TDX signer attestation header length: magic + timestamp + 3 lengths.
const TDX_SIGNER_ATTESTATION_HEADER_LEN: usize = TDX_SIGNER_ATTESTATION_MAGIC.len() + 8 + 8 + 8 + 8;

/// Self-contained TDX signer attestation returned by `enclave_signerAttestation`.
///
/// Wire format:
/// - 8 bytes: [`TDX_SIGNER_ATTESTATION_MAGIC`]
/// - 8 bytes: quote timestamp in little-endian milliseconds
/// - 8 bytes: signer public key byte length in little-endian
/// - 8 bytes: quote byte length in little-endian
/// - 8 bytes: registrar nonce byte length in little-endian
/// - registrar nonce bytes: empty or a 32-byte deterministic registrar nonce
/// - public key bytes: expected uncompressed secp256k1 signer public key
/// - quote bytes: raw TDX quote
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TdxSignerAttestation {
    /// Expected uncompressed secp256k1 signer public key: `0x04 || x || y`.
    pub signer_public_key: Bytes,
    /// Raw Intel TDX quote bytes.
    pub quote: Bytes,
    /// Quote collection timestamp in milliseconds since Unix epoch.
    ///
    /// This value is committed into `TDREPORT.REPORTDATA` by
    /// `base-proof-tee-tdx-runtime` and must be supplied to the verifier.
    pub quote_timestamp_millis: u64,
    /// Optional deterministic registrar nonce bound into `TDREPORT.REPORTDATA`.
    pub attestation_nonce: Option<B256>,
}

impl TdxSignerAttestation {
    /// Encodes this attestation into the JSON-RPC byte payload format.
    pub fn encode(&self) -> Vec<u8> {
        let nonce = self.attestation_nonce.map_or_else(Vec::new, |nonce| nonce.to_vec());
        let mut encoded = Vec::with_capacity(
            TDX_SIGNER_ATTESTATION_HEADER_LEN
                + nonce.len()
                + self.signer_public_key.len()
                + self.quote.len(),
        );
        encoded.extend_from_slice(TDX_SIGNER_ATTESTATION_MAGIC);
        encoded.extend_from_slice(&self.quote_timestamp_millis.to_le_bytes());
        encoded.extend_from_slice(&(self.signer_public_key.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&(self.quote.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&(nonce.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&nonce);
        encoded.extend_from_slice(&self.signer_public_key);
        encoded.extend_from_slice(&self.quote);
        encoded
    }

    /// Decodes a JSON-RPC TDX signer attestation byte payload.
    pub fn decode(encoded: &[u8]) -> Result<Self, TdxSignerAttestationDecodeError> {
        if encoded.len() < TDX_SIGNER_ATTESTATION_HEADER_LEN {
            return Err(TdxSignerAttestationDecodeError::HeaderTooShort { len: encoded.len() });
        }
        if !encoded.starts_with(TDX_SIGNER_ATTESTATION_MAGIC) {
            return Err(TdxSignerAttestationDecodeError::InvalidMagic);
        }

        let quote_timestamp_millis = Self::read_le_u64(&encoded[8..16]);
        let public_key_len_u64 = Self::read_le_u64(&encoded[16..24]);
        let quote_len_u64 = Self::read_le_u64(&encoded[24..32]);
        let nonce_len_u64 = Self::read_le_u64(&encoded[32..40]);

        let public_key_len = usize::try_from(public_key_len_u64).map_err(|_| {
            TdxSignerAttestationDecodeError::LengthOverflow {
                field: "public_key",
                len: public_key_len_u64,
            }
        })?;
        let quote_len = usize::try_from(quote_len_u64).map_err(|_| {
            TdxSignerAttestationDecodeError::LengthOverflow { field: "quote", len: quote_len_u64 }
        })?;
        let nonce_len = usize::try_from(nonce_len_u64).map_err(|_| {
            TdxSignerAttestationDecodeError::LengthOverflow { field: "nonce", len: nonce_len_u64 }
        })?;

        let expected_len = TDX_SIGNER_ATTESTATION_HEADER_LEN
            .checked_add(nonce_len)
            .and_then(|len| len.checked_add(public_key_len))
            .and_then(|len| len.checked_add(quote_len))
            .ok_or_else(|| TdxSignerAttestationDecodeError::LengthOverflow {
                field: "payload",
                len: (TDX_SIGNER_ATTESTATION_HEADER_LEN as u64)
                    .saturating_add(nonce_len_u64)
                    .saturating_add(public_key_len_u64)
                    .saturating_add(quote_len_u64),
            })?;
        if encoded.len() != expected_len {
            return Err(TdxSignerAttestationDecodeError::LengthMismatch {
                expected: expected_len,
                actual: encoded.len(),
            });
        }

        let nonce_start = TDX_SIGNER_ATTESTATION_HEADER_LEN;
        let public_key_start = nonce_start + nonce_len;
        let quote_start = public_key_start + public_key_len;
        let attestation_nonce = match nonce_len {
            0 => None,
            32 => Some(B256::from_slice(&encoded[nonce_start..public_key_start])),
            _ => {
                return Err(TdxSignerAttestationDecodeError::InvalidNonceLength { len: nonce_len });
            }
        };
        Ok(Self {
            signer_public_key: Bytes::copy_from_slice(&encoded[public_key_start..quote_start]),
            quote: Bytes::copy_from_slice(&encoded[quote_start..]),
            quote_timestamp_millis,
            attestation_nonce,
        })
    }

    fn read_le_u64(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes.try_into().expect("caller guarantees 8 bytes"))
    }
}

/// Error returned when decoding a TDX signer attestation payload fails.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TdxSignerAttestationDecodeError {
    /// Encoded payload is shorter than the fixed header.
    #[error("TDX signer attestation header is too short: {len} bytes")]
    HeaderTooShort {
        /// Actual payload length.
        len: usize,
    },
    /// Encoded payload does not start with the expected magic bytes.
    #[error("TDX signer attestation magic prefix is invalid")]
    InvalidMagic,
    /// Encoded field length cannot fit the current platform.
    #[error("TDX signer attestation {field} length overflows usize: {len}")]
    LengthOverflow {
        /// Payload field whose encoded length overflowed.
        field: &'static str,
        /// Encoded field length.
        len: u64,
    },
    /// The encoded registrar nonce was neither absent nor 32 bytes.
    #[error("TDX signer attestation nonce length must be 0 or 32 bytes, got {len}")]
    InvalidNonceLength {
        /// Invalid nonce length.
        len: usize,
    },
    /// Encoded payload length does not match the embedded quote length.
    #[error("TDX signer attestation length mismatch: expected {expected} bytes, got {actual}")]
    LengthMismatch {
        /// Expected payload length from the embedded quote length.
        expected: usize,
        /// Actual payload length.
        actual: usize,
    },
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;

    use super::*;

    fn fixture_attestation() -> TdxSignerAttestation {
        TdxSignerAttestation {
            signer_public_key: Bytes::from_static(b"fixture-public-key"),
            quote: Bytes::from_static(b"fixture-quote"),
            quote_timestamp_millis: 1_711_111_111_000,
            attestation_nonce: Some(B256::repeat_byte(0x11)),
        }
    }

    #[test]
    fn signer_attestation_round_trips() {
        let attestation = fixture_attestation();
        let encoded = attestation.encode();

        assert_eq!(TdxSignerAttestation::decode(&encoded).unwrap(), attestation);
    }

    #[test]
    fn signer_attestation_decode_rejects_invalid_magic() {
        let attestation = fixture_attestation();
        let mut encoded = attestation.encode();
        encoded[0] = b'X';

        assert_eq!(
            TdxSignerAttestation::decode(&encoded).unwrap_err(),
            TdxSignerAttestationDecodeError::InvalidMagic
        );
    }

    #[test]
    fn signer_attestation_decode_rejects_length_mismatch() {
        let attestation = fixture_attestation();
        let mut encoded = attestation.encode();
        let expected = encoded.len();
        encoded.pop();

        assert_eq!(
            TdxSignerAttestation::decode(&encoded).unwrap_err(),
            TdxSignerAttestationDecodeError::LengthMismatch { expected, actual: encoded.len() }
        );
    }

    #[test]
    fn signer_attestation_decode_rejects_invalid_nonce_length() {
        let mut encoded = fixture_attestation().encode();
        encoded[32..40].copy_from_slice(&1u64.to_le_bytes());
        encoded.drain(41..72);

        assert_eq!(
            TdxSignerAttestation::decode(&encoded).unwrap_err(),
            TdxSignerAttestationDecodeError::InvalidNonceLength { len: 1 }
        );
    }
}
