//! Serde utilities for Go compatibility.
//!
//! These modules provide serialization that matches Go's `hexutil` package exactly.

/// Serialization for `hexutil.Bytes` compatibility.
///
/// Serializes bytes as `0x`-prefixed lowercase hex string.
pub mod bytes_hex {
    use alloy_primitives::Bytes;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_string = format!("0x{}", hex::encode(bytes.as_ref()));
        serializer.serialize_str(&hex_string)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        Ok(Bytes::from(bytes))
    }
}

/// Serialization for `hexutil.Big` compatibility.
///
/// Serializes U256 as minimal `0x`-prefixed hex string (no leading zeros).
/// For example, 12345 serializes as `"0x3039"`, not `"0x0000...3039"`.
pub mod u256_hex {
    use alloy_primitives::U256;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &U256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if value.is_zero() {
            return serializer.serialize_str("0x0");
        }

        // Convert to hex and strip leading zeros
        let hex_string = format!("0x{value:x}");
        serializer.serialize_str(&hex_string)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);

        if s.is_empty() || s == "0" {
            return Ok(U256::ZERO);
        }

        U256::from_str_radix(s, 16).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, U256};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestBytes {
        #[serde(with = "bytes_hex")]
        data: Bytes,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestU256 {
        #[serde(with = "u256_hex")]
        value: U256,
    }

    #[test]
    fn test_bytes_hex_serialize() {
        let test = TestBytes {
            data: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
        };
        let json = serde_json::to_string(&test).unwrap();
        assert_eq!(json, r#"{"data":"0xdeadbeef"}"#);
    }

    #[test]
    fn test_bytes_hex_deserialize() {
        let json = r#"{"data":"0xdeadbeef"}"#;
        let test: TestBytes = serde_json::from_str(json).unwrap();
        assert_eq!(test.data.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn test_bytes_hex_empty() {
        let test = TestBytes { data: Bytes::new() };
        let json = serde_json::to_string(&test).unwrap();
        assert_eq!(json, r#"{"data":"0x"}"#);

        let parsed: TestBytes = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, test);
    }

    #[test]
    fn test_u256_hex_serialize() {
        let test = TestU256 {
            value: U256::from(12345),
        };
        let json = serde_json::to_string(&test).unwrap();
        assert_eq!(json, r#"{"value":"0x3039"}"#);
    }

    #[test]
    fn test_u256_hex_serialize_zero() {
        let test = TestU256 { value: U256::ZERO };
        let json = serde_json::to_string(&test).unwrap();
        assert_eq!(json, r#"{"value":"0x0"}"#);
    }

    #[test]
    fn test_u256_hex_serialize_large() {
        let test = TestU256 {
            value: U256::from(0x123456789abcdef0_u64),
        };
        let json = serde_json::to_string(&test).unwrap();
        assert_eq!(json, r#"{"value":"0x123456789abcdef0"}"#);
    }

    #[test]
    fn test_u256_hex_deserialize() {
        let json = r#"{"value":"0x3039"}"#;
        let test: TestU256 = serde_json::from_str(json).unwrap();
        assert_eq!(test.value, U256::from(12345));
    }

    #[test]
    fn test_u256_hex_roundtrip() {
        let original = TestU256 {
            value: U256::from(0xdeadbeef_u64),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: TestU256 = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}
