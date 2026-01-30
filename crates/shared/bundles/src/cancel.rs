//! Types for bundle cancellation and identification.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// Response containing a bundle hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BundleHash {
    /// The hash identifying the bundle.
    pub bundle_hash: B256,
}

/// Request to cancel a bundle by its replacement UUID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CancelBundle {
    /// The replacement UUID of the bundle to cancel.
    pub replacement_uuid: String,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;

    use super::*;

    #[test]
    fn test_bundle_hash_serialization() {
        let hash = BundleHash {
            bundle_hash: b256!(
                "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
            ),
        };

        let json = serde_json::to_string(&hash).unwrap();
        assert!(json.contains(
            "\"bundleHash\":\"0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef\""
        ));

        let deserialized: BundleHash = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, hash);
    }

    #[test]
    fn test_bundle_hash_deserialization() {
        let json = r#"{"bundleHash":"0x0000000000000000000000000000000000000000000000000000000000000001"}"#;
        let hash: BundleHash = serde_json::from_str(json).unwrap();
        assert_eq!(
            hash.bundle_hash,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000001")
        );
    }

    #[test]
    fn test_cancel_bundle_serialization() {
        let cancel =
            CancelBundle { replacement_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string() };

        let json = serde_json::to_string(&cancel).unwrap();
        assert!(json.contains("\"replacementUuid\":\"550e8400-e29b-41d4-a716-446655440000\""));

        let deserialized: CancelBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, cancel);
    }

    #[test]
    fn test_cancel_bundle_deserialization() {
        let json = r#"{"replacementUuid":"test-uuid-12345"}"#;
        let cancel: CancelBundle = serde_json::from_str(json).unwrap();
        assert_eq!(cancel.replacement_uuid, "test-uuid-12345");
    }

    #[test]
    fn test_bundle_hash_debug() {
        let hash = BundleHash { bundle_hash: B256::default() };
        let debug = format!("{hash:?}");
        assert!(debug.contains("BundleHash"));
    }

    #[test]
    fn test_cancel_bundle_debug() {
        let cancel = CancelBundle { replacement_uuid: "test".to_string() };
        let debug = format!("{cancel:?}");
        assert!(debug.contains("CancelBundle"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn test_bundle_hash_clone() {
        let hash = BundleHash {
            bundle_hash: b256!(
                "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
            ),
        };
        let cloned = hash.clone();
        assert_eq!(hash, cloned);
    }

    #[test]
    fn test_cancel_bundle_clone() {
        let cancel = CancelBundle { replacement_uuid: "my-uuid".to_string() };
        let cloned = cancel.clone();
        assert_eq!(cancel, cloned);
    }
}
