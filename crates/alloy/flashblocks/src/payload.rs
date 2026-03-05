//! Contains payload types.

use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::Withdrawal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Represents the modified portions of an execution payload within a flashblock.
/// This structure contains only the fields that can be updated during block construction,
/// such as state root, receipts, logs, and new transactions. Other immutable block fields
/// like parent hash and block number are excluded since they remain constant throughout
/// the block's construction.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct ExecutionPayloadFlashblockDeltaV1 {
    /// The state root of the block.
    pub state_root: B256,
    /// The receipts root of the block.
    pub receipts_root: B256,
    /// The logs bloom of the block.
    pub logs_bloom: Bloom,
    /// The gas used of the block.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// The block hash of the block.
    pub block_hash: B256,
    /// The transactions of the block.
    pub transactions: Vec<Bytes>,
    /// Array of [`Withdrawal`] enabled with V2
    pub withdrawals: Vec<Withdrawal>,
    /// The withdrawals root of the block.
    pub withdrawals_root: B256,
    /// The blob gas used
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub blob_gas_used: Option<u64>,
}

/// Represents the base configuration of an execution payload that remains constant
/// throughout block construction. This includes fundamental block properties like
/// parent hash, block number, and other header fields that are determined at
/// block creation and cannot be modified.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct ExecutionPayloadBaseV1 {
    /// Ecotone parent beacon block root
    pub parent_beacon_block_root: B256,
    /// The parent hash of the block.
    pub parent_hash: B256,
    /// The fee recipient of the block.
    pub fee_recipient: Address,
    /// The previous randao of the block.
    pub prev_randao: B256,
    /// The block number.
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,
    /// The gas limit of the block.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_limit: u64,
    /// The timestamp of the block.
    #[serde(with = "alloy_serde::quantity")]
    pub timestamp: u64,
    /// The extra data of the block.
    pub extra_data: Bytes,
    /// The base fee per gas of the block.
    pub base_fee_per_gas: U256,
}

/// Represents a flashblock payload containing the base execution payload configuration,
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct FlashblocksPayloadV1 {
    /// The payload id of the flashblock
    pub payload_id: PayloadId,
    /// The index of the flashblock in the block
    pub index: u64,
    /// The base execution payload configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<ExecutionPayloadBaseV1>,
    /// The delta/diff containing modified portions of the execution payload
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    /// Additional metadata associated with the flashblock
    pub metadata: Value,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::PayloadId;
    use serde_json::json;

    use super::*;

    /// Pin the JSON field names and serde encoding of [`ExecutionPayloadBaseV1`].
    ///
    /// Any change to field names, serde attributes, or encoding (e.g. switching
    /// `block_number` from hex-quantity to plain integer) will break downstream
    /// consumers parsing the flashblocks websocket.
    #[test]
    fn base_v1_json_format_is_stable() {
        let base = ExecutionPayloadBaseV1 {
            parent_beacon_block_root: B256::from([0x01; 32]),
            parent_hash: B256::from([0x02; 32]),
            fee_recipient: Address::from([0xAA; 20]),
            prev_randao: B256::from([0x03; 32]),
            block_number: 100,
            gas_limit: 30_000_000,
            timestamp: 1_700_000_000,
            extra_data: Bytes::from(vec![0xBE, 0xEF]),
            base_fee_per_gas: U256::from(1_000_000_000u64),
        };

        let json = serde_json::to_value(&base).unwrap();
        let obj = json.as_object().unwrap();

        // Exact field set — adding or removing fields is a breaking change
        let expected_fields: BTreeSet<&str> = [
            "parent_beacon_block_root",
            "parent_hash",
            "fee_recipient",
            "prev_randao",
            "block_number",
            "gas_limit",
            "timestamp",
            "extra_data",
            "base_fee_per_gas",
        ]
        .into();
        let actual_fields: BTreeSet<&str> = obj.keys().map(|s| s.as_str()).collect();
        assert_eq!(actual_fields, expected_fields, "field names changed");

        // Quantity-encoded fields must be hex strings
        assert_eq!(obj["block_number"], json!("0x64"));
        assert_eq!(obj["gas_limit"], json!("0x1c9c380"));
        assert_eq!(obj["timestamp"], json!("0x6553f100"));

        // Round-trip
        let deserialized: ExecutionPayloadBaseV1 = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(deserialized, base);
    }

    /// Pin the JSON field names and serde encoding of [`ExecutionPayloadFlashblockDeltaV1`].
    #[test]
    fn delta_v1_json_format_is_stable() {
        let delta = ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::from([0x04; 32]),
            receipts_root: B256::from([0x05; 32]),
            logs_bloom: Bloom::default(),
            gas_used: 21_000,
            block_hash: B256::from([0x06; 32]),
            transactions: vec![Bytes::from(vec![0x01, 0x02])],
            withdrawals: Vec::new(),
            withdrawals_root: B256::from([0x07; 32]),
            blob_gas_used: Some(131_072),
        };

        let json = serde_json::to_value(&delta).unwrap();
        let obj = json.as_object().unwrap();

        let expected_fields: BTreeSet<&str> = [
            "state_root",
            "receipts_root",
            "logs_bloom",
            "gas_used",
            "block_hash",
            "transactions",
            "withdrawals",
            "withdrawals_root",
            "blob_gas_used",
        ]
        .into();
        let actual_fields: BTreeSet<&str> = obj.keys().map(|s| s.as_str()).collect();
        assert_eq!(actual_fields, expected_fields, "field names changed");

        assert_eq!(obj["gas_used"], json!("0x5208"));
        assert_eq!(obj["blob_gas_used"], json!("0x20000"));

        let deserialized: ExecutionPayloadFlashblockDeltaV1 =
            serde_json::from_value(json.clone()).unwrap();
        assert_eq!(deserialized, delta);
    }

    /// `blob_gas_used` must be omitted (not `null`) when `None`.
    #[test]
    fn delta_v1_omits_blob_gas_used_when_none() {
        let delta = ExecutionPayloadFlashblockDeltaV1 { blob_gas_used: None, ..Default::default() };

        let json = serde_json::to_value(&delta).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("blob_gas_used"),
            "blob_gas_used should be omitted when None, not serialized as null"
        );
    }

    /// `base` must be omitted (not `null`) when `None` in the top-level payload.
    #[test]
    fn payload_v1_omits_base_when_none() {
        let payload = FlashblocksPayloadV1 {
            payload_id: PayloadId::new([0x01; 8]),
            index: 1,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: json!({"block_number": 1}),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("base"),
            "base should be omitted when None, not serialized as null"
        );
    }

    /// Pin the top-level [`FlashblocksPayloadV1`] field set.
    #[test]
    fn payload_v1_json_format_is_stable() {
        let payload = FlashblocksPayloadV1 {
            payload_id: PayloadId::new([0x01; 8]),
            index: 0,
            base: Some(ExecutionPayloadBaseV1::default()),
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: json!({"block_number": 42}),
        };

        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().unwrap();

        let expected_fields: BTreeSet<&str> =
            ["payload_id", "index", "base", "diff", "metadata"].into();
        let actual_fields: BTreeSet<&str> = obj.keys().map(|s| s.as_str()).collect();
        assert_eq!(actual_fields, expected_fields, "top-level field names changed");

        // index is a plain u64, not hex-encoded
        assert_eq!(obj["index"], json!(0));

        let deserialized: FlashblocksPayloadV1 = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(deserialized, payload);
    }
}
