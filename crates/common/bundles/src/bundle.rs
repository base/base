//! Raw bundle type for API requests.

use alloy_primitives::Bytes;
use serde::{Deserialize, Serialize};

/// `Bundle` is the transaction-batch container used for metering and audit.
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    /// The raw transaction bytes in the bundle.
    pub txs: Vec<Bytes>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_default() {
        let bundle = Bundle::default();
        assert!(bundle.txs.is_empty());
    }

    #[test]
    fn test_bundle_serialization() {
        let bundle = Bundle { txs: vec![] };

        let json = serde_json::to_string(&bundle).unwrap();
        assert_eq!(json, r#"{"txs":[]}"#);

        let deserialized: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, bundle);
    }

    #[test]
    fn test_bundle_deserialization_ignores_unknown_fields() {
        let json = r#"{
            "txs": [],
            "minBlockNumber": "0x1",
            "maxBlockNumber": "0x2"
        }"#;

        let bundle: Bundle = serde_json::from_str(json).unwrap();
        assert!(bundle.txs.is_empty());
    }

    #[test]
    fn test_bundle_clone_and_eq() {
        let bundle = Bundle { txs: vec![] };

        let cloned = bundle.clone();
        assert_eq!(bundle, cloned);
    }
}
