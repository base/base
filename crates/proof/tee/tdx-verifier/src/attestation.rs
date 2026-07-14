//! Confidential Space signer attestation payload encoding.

use alloy_primitives::{Address, B256, Bytes};
use thiserror::Error;

/// Magic prefix for encoded TDX signer attestations returned by JSON-RPC.
const TDX_SIGNER_ATTESTATION_MAGIC: &[u8; 8] = b"BASECS01";

/// Encoded attestation header length.
const TDX_SIGNER_ATTESTATION_HEADER_LEN: usize =
    TDX_SIGNER_ATTESTATION_MAGIC.len() + 8 + 20 + 8 + 8 + 8;

/// Self-contained Confidential Space signer attestation returned by `enclave_signerAttestation`.
///
/// Wire format:
/// - 8 bytes: [`TDX_SIGNER_ATTESTATION_MAGIC`]
/// - 8 bytes: L1 chain ID in little-endian
/// - 20 bytes: `TEEProverRegistry` address
/// - 8 bytes: signer public key byte length in little-endian
/// - 8 bytes: token byte length in little-endian
/// - 8 bytes: registrar nonce byte length in little-endian
/// - registrar nonce bytes: empty or a 32-byte deterministic registrar nonce
/// - public key bytes: expected uncompressed secp256k1 signer public key
/// - token bytes: Google Cloud Attestation PKI token
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TdxSignerAttestation {
    /// Expected uncompressed secp256k1 signer public key: `0x04 || x || y`.
    pub signer_public_key: Bytes,
    /// Google Cloud Attestation PKI token returned by the Confidential Space launcher.
    pub token: Bytes,
    /// Optional deterministic registrar nonce bound into the token.
    pub attestation_nonce: Option<B256>,
    /// L1 chain ID bound into the token nonce.
    pub chain_id: u64,
    /// `TEEProverRegistry` address bound into the token nonce.
    pub registry_address: Address,
}

impl TdxSignerAttestation {
    /// Encodes this attestation into the JSON-RPC byte payload format.
    pub fn encode(&self) -> Vec<u8> {
        let nonce = self.attestation_nonce.map_or_else(Vec::new, |nonce| nonce.to_vec());
        let mut encoded = Vec::with_capacity(
            TDX_SIGNER_ATTESTATION_HEADER_LEN
                + nonce.len()
                + self.signer_public_key.len()
                + self.token.len(),
        );
        encoded.extend_from_slice(TDX_SIGNER_ATTESTATION_MAGIC);
        encoded.extend_from_slice(&self.chain_id.to_le_bytes());
        encoded.extend_from_slice(self.registry_address.as_slice());
        encoded.extend_from_slice(&(self.signer_public_key.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&(self.token.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&(nonce.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&nonce);
        encoded.extend_from_slice(&self.signer_public_key);
        encoded.extend_from_slice(&self.token);
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

        let chain_id = Self::read_le_u64(&encoded[8..16]);
        let registry_address = Address::from_slice(&encoded[16..36]);
        let public_key_len_u64 = Self::read_le_u64(&encoded[36..44]);
        let token_len_u64 = Self::read_le_u64(&encoded[44..52]);
        let nonce_len_u64 = Self::read_le_u64(&encoded[52..60]);
        let public_key_len = Self::length("public_key", public_key_len_u64)?;
        let token_len = Self::length("token", token_len_u64)?;
        let nonce_len = Self::length("nonce", nonce_len_u64)?;

        let expected_len = TDX_SIGNER_ATTESTATION_HEADER_LEN
            .checked_add(nonce_len)
            .and_then(|len| len.checked_add(public_key_len))
            .and_then(|len| len.checked_add(token_len))
            .ok_or_else(|| TdxSignerAttestationDecodeError::LengthOverflow {
                field: "payload",
                len: (TDX_SIGNER_ATTESTATION_HEADER_LEN as u64)
                    .saturating_add(nonce_len_u64)
                    .saturating_add(public_key_len_u64)
                    .saturating_add(token_len_u64),
            })?;
        if encoded.len() != expected_len {
            return Err(TdxSignerAttestationDecodeError::LengthMismatch {
                expected: expected_len,
                actual: encoded.len(),
            });
        }

        let nonce_start = TDX_SIGNER_ATTESTATION_HEADER_LEN;
        let public_key_start = nonce_start + nonce_len;
        let token_start = public_key_start + public_key_len;
        let attestation_nonce = match nonce_len {
            0 => None,
            32 => Some(B256::from_slice(&encoded[nonce_start..public_key_start])),
            _ => {
                return Err(TdxSignerAttestationDecodeError::InvalidNonceLength { len: nonce_len });
            }
        };
        Ok(Self {
            signer_public_key: Bytes::copy_from_slice(&encoded[public_key_start..token_start]),
            token: Bytes::copy_from_slice(&encoded[token_start..]),
            attestation_nonce,
            chain_id,
            registry_address,
        })
    }

    fn length(field: &'static str, value: u64) -> Result<usize, TdxSignerAttestationDecodeError> {
        usize::try_from(value)
            .map_err(|_| TdxSignerAttestationDecodeError::LengthOverflow { field, len: value })
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
    /// Encoded payload length does not match the embedded token length.
    #[error("TDX signer attestation length mismatch: expected {expected} bytes, got {actual}")]
    LengthMismatch {
        /// Expected payload length from the embedded token length.
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
            token: Bytes::from_static(b"fixture-token"),
            attestation_nonce: Some(B256::repeat_byte(0x11)),
            chain_id: 11_155_111,
            registry_address: Address::repeat_byte(0x33),
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
        let mut encoded = fixture_attestation().encode();
        encoded[0] = b'X';

        assert_eq!(
            TdxSignerAttestation::decode(&encoded).unwrap_err(),
            TdxSignerAttestationDecodeError::InvalidMagic
        );
    }
}
