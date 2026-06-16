use base64::Engine;
use rand::RngCore;

use crate::error::Error;

const GENERIC_COMMITMENT_TYPE: u8 = 0x01;
/// Max decoded commitment bytes (generic format is always 34).
const MAX_COMMITMENT_LEN: usize = 34;

/// Server-generated generic commitment (`0x01` type byte + `0xff` sentinel + random suffix).
pub fn generate_generic_commitment() -> Vec<u8> {
    let mut commitment = [0u8; 34];
    commitment[0] = GENERIC_COMMITMENT_TYPE;
    commitment[1] = 0xff;
    rand::rng().fill_bytes(&mut commitment[2..]);
    commitment.to_vec()
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
        let hex = format!("0x{}", hex::encode(&comm));
        assert_eq!(decode_hex_commitment(&hex).unwrap(), comm);
    }

    #[test]
    fn rejects_oversized_commitment_hex() {
        let hex = format!("0x{}", "ab".repeat(MAX_COMMITMENT_LEN + 1));
        let err = decode_hex_commitment(&hex).unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }
}
