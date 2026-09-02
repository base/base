//! Hex formatting for the string mirrors of the BYTEA hash columns.

use alloy_primitives::hex;

/// Formats block hashes for the `hash_hex` and `canonical_hash_hex` columns.
///
/// The writer stores these and the reader looks rows up by string equality, so the two must agree
/// on one spelling of a hash or a lookup silently misses instead of failing. That spelling is
/// `0x`-prefixed lowercase hex: it is what the shadow-metrics JSON API already emits, it is how
/// every other EVM hash in Snowflake is written, and `shadow_blocks_hash_hex_format` rejects
/// anything else at the database.
#[derive(Clone, Copy, Debug)]
pub struct ShadowHash;

impl ShadowHash {
    /// Formats raw hash bytes as `0x`-prefixed lowercase hex.
    #[must_use]
    pub fn encode(bytes: &[u8]) -> String {
        hex::encode_prefixed(bytes)
    }

    /// Formats an absent-or-present hash, preserving absence.
    ///
    /// `canonical_hash` is NULL until the chain names a replacement, and the hex mirror has to
    /// stay NULL with it: an empty string would read as a resolved row that points nowhere.
    #[must_use]
    pub fn encode_optional(bytes: Option<&Vec<u8>>) -> Option<String> {
        bytes.map(|bytes| Self::encode(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::ShadowHash;

    #[test]
    fn encodes_lowercase_with_an_0x_prefix() {
        assert_eq!(ShadowHash::encode(&[0xAB, 0xCD, 0xEF]), "0xabcdef");
    }

    #[test]
    fn encodes_a_full_block_hash_to_the_width_the_check_constraint_expects() {
        let encoded = ShadowHash::encode(&[0x0f; 32]);
        assert_eq!(encoded.len(), 66, "0x plus 64 hex digits");
        assert!(encoded.starts_with("0x0f0f"));
    }

    #[test]
    fn absent_hashes_stay_absent() {
        assert_eq!(ShadowHash::encode_optional(None), None);
        assert_eq!(ShadowHash::encode_optional(Some(&vec![0x01])), Some("0x01".to_string()));
    }
}
