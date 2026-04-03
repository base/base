//! Raw bundle type for API requests.

use alloy_primitives::{Bytes, TxHash};
use serde::{Deserialize, Serialize};

/// `Bundle` is the type that mirrors `EthSendBundle` and is used for the API.
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    /// The raw transaction bytes in the bundle.
    pub txs: Vec<Bytes>,

    /// The target block number for inclusion.
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,

    /// Minimum flashblock number for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_min: Option<u64>,

    /// Maximum flashblock number for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_max: Option<u64>,

    /// Minimum timestamp for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_timestamp: Option<u64>,

    /// Maximum timestamp for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_timestamp: Option<u64>,

    /// Transaction hashes that are allowed to revert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverting_tx_hashes: Vec<TxHash>,

    /// UUID for bundle replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_uuid: Option<String>,

    /// Transaction hashes that should be dropped from the pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropping_tx_hashes: Vec<TxHash>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_default() {
        let bundle = Bundle::default();
        assert!(bundle.txs.is_empty());
        assert_eq!(bundle.block_number, 0);
        assert!(bundle.flashblock_number_min.is_none());
        assert!(bundle.flashblock_number_max.is_none());
        assert!(bundle.min_timestamp.is_none());
        assert!(bundle.max_timestamp.is_none());
        assert!(bundle.reverting_tx_hashes.is_empty());
        assert!(bundle.replacement_uuid.is_none());
        assert!(bundle.dropping_tx_hashes.is_empty());
    }

    #[test]
    fn test_bundle_serialization() {
        let bundle = Bundle {
            txs: vec![],
            block_number: 12345,
            flashblock_number_min: Some(1),
            flashblock_number_max: Some(5),
            min_timestamp: Some(1000),
            max_timestamp: Some(2000),
            reverting_tx_hashes: vec![],
            replacement_uuid: Some("test-uuid".to_string()),
            dropping_tx_hashes: vec![],
        };

        let json = serde_json::to_string(&bundle).unwrap();
        assert!(json.contains("\"blockNumber\":\"0x3039\""));
        // Optional fields serialize as integers, not hex
        assert!(json.contains("\"flashblockNumberMin\":1"));
        assert!(json.contains("\"flashblockNumberMax\":5"));
        assert!(json.contains("\"replacementUuid\":\"test-uuid\""));

        let deserialized: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, bundle);
    }

    #[test]
    fn test_bundle_deserialization_minimal() {
        let json = r#"{
            "txs": [],
            "blockNumber": "0x1"
        }"#;

        let bundle: Bundle = serde_json::from_str(json).unwrap();
        assert_eq!(bundle.block_number, 1);
        assert!(bundle.flashblock_number_min.is_none());
        assert!(bundle.flashblock_number_max.is_none());
        assert!(bundle.min_timestamp.is_none());
        assert!(bundle.max_timestamp.is_none());
    }

    #[test]
    fn test_bundle_clone_and_eq() {
        let bundle = Bundle { txs: vec![], block_number: 100, ..Default::default() };

        let cloned = bundle.clone();
        assert_eq!(bundle, cloned);
    }
}
