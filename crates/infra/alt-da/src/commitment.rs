use alloy_primitives::Bytes;
use base_protocol::DERIVATION_VERSION_1;
use base64::Engine;
use rand::RngCore;

use crate::error::{CommitmentError, Error};

/// Generic commitment type byte (`0x01`).
pub const GENERIC_COMMITMENT_TYPE: u8 = 0x01;
/// Generic commitment sentinel byte (`0xff`).
pub const GENERIC_COMMITMENT_SENTINEL: u8 = 0xff;
/// Encoded generic commitment length in bytes.
///
/// Single source of truth: [`base_protocol::GENERIC_COMMITMENT_LEN`].
pub const GENERIC_COMMITMENT_LEN: usize = base_protocol::GENERIC_COMMITMENT_LEN;
/// Max decoded commitment bytes (generic format is always 34).
const MAX_COMMITMENT_LEN: usize = GENERIC_COMMITMENT_LEN;

/// Fixed-size generic commitment: `0x01` type byte, `0xff` sentinel, then 32 random bytes.
pub type GenericCommitment = [u8; GENERIC_COMMITMENT_LEN];

/// Server-generated generic commitment (`0x01` type byte + `0xff` sentinel + random suffix).
pub fn generate_generic_commitment() -> GenericCommitment {
    let mut commitment = [0u8; GENERIC_COMMITMENT_LEN];
    commitment[0] = GENERIC_COMMITMENT_TYPE;
    commitment[1] = GENERIC_COMMITMENT_SENTINEL;
    rand::rng().fill_bytes(&mut commitment[2..]);
    commitment
}

/// Validate a server-returned generic commitment.
pub fn validate_generic_commitment(commitment: &[u8]) -> Result<(), CommitmentError> {
    if commitment.len() != GENERIC_COMMITMENT_LEN {
        return Err(CommitmentError::InvalidLength { len: commitment.len() });
    }
    if commitment[0] != GENERIC_COMMITMENT_TYPE || commitment[1] != GENERIC_COMMITMENT_SENTINEL {
        return Err(CommitmentError::InvalidPrefix);
    }
    Ok(())
}

/// Encode alt-DA commitment L1 calldata (`DERIVATION_VERSION_1` ++ commitment).
///
/// Infallible: a [`GenericCommitment`] is fixed-size and its prefix is set at
/// construction (`generate_generic_commitment`) or validated at the client boundary
/// (`Client::put`), so there is nothing left to reject here.
pub fn encode_commitment_tx_data(commitment: GenericCommitment) -> Bytes {
    let mut data = Vec::with_capacity(1 + commitment.len());
    data.push(DERIVATION_VERSION_1);
    data.extend_from_slice(&commitment);
    Bytes::from(data)
}

/// Decode a `0x`-prefixed hex commitment from an HTTP path segment.
pub fn decode_hex_commitment(hex: &str) -> Result<Vec<u8>, Error> {
    let stripped = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X")).unwrap_or(hex);
    if stripped.is_empty() {
        return Err(Error::BadRequest("empty commitment hex".into()));
    }
    let bytes = hex::decode(stripped)
        .map_err(|e| Error::BadRequest(format!("invalid commitment hex: {e}")))?;
    if bytes.len() > MAX_COMMITMENT_LEN {
        return Err(Error::BadRequest(format!(
            "commitment too large: {} bytes (max {MAX_COMMITMENT_LEN})",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Base64url object name for an encoded commitment key.
pub fn object_name(commitment: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(commitment)
}

/// Join store path prefix with the encoded commitment object name.
pub fn object_key(prefix: &str, commitment: &[u8]) -> String {
    let name = object_name(commitment);
    let prefix = prefix.trim_start_matches('/');
    if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_commitment_has_expected_prefix() {
        let comm = generate_generic_commitment();
        assert_eq!(comm[0], GENERIC_COMMITMENT_TYPE);
        assert_eq!(comm[1], 0xff);
        assert_eq!(comm.len(), 34);
    }

    #[test]
    fn roundtrip_hex_commitment() {
        let comm = generate_generic_commitment();
        let hex = format!("0x{}", hex::encode(comm));
        assert_eq!(decode_hex_commitment(&hex).unwrap().as_slice(), comm.as_slice());
    }

    #[test]
    fn rejects_oversized_commitment_hex() {
        let hex = format!("0x{}", "ab".repeat(MAX_COMMITMENT_LEN + 1));
        let err = decode_hex_commitment(&hex).unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn validate_accepts_generated_commitment() {
        assert!(validate_generic_commitment(&generate_generic_commitment()).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_length() {
        let err = validate_generic_commitment(&[GENERIC_COMMITMENT_TYPE; 10]).unwrap_err();
        assert!(matches!(err, CommitmentError::InvalidLength { len: 10 }));
    }

    #[test]
    fn validate_rejects_bad_prefix() {
        let mut comm = generate_generic_commitment();
        comm[1] = 0x00;
        let err = validate_generic_commitment(&comm).unwrap_err();
        assert!(matches!(err, CommitmentError::InvalidPrefix));
    }

    #[test]
    fn encode_commitment_tx_data_prefixes_derivation_version() {
        let comm = generate_generic_commitment();
        let tx_data = encode_commitment_tx_data(comm);
        assert_eq!(tx_data[0], DERIVATION_VERSION_1);
        assert_eq!(&tx_data[1..], comm.as_slice());
    }
}
