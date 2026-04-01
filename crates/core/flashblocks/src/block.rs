//! Contains the [`Flashblock`] type used in Flashblocks.

use std::io::Read;

use alloy_rpc_types_engine::PayloadId;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblockDecodeError,
    FlashblocksPayloadV1, Metadata,
};

/// A flashblock containing partial block data.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct Flashblock {
    /// Unique payload identifier.
    pub payload_id: PayloadId,
    /// Index of this flashblock within the block.
    pub index: u64,
    /// Base payload data (only present on first flashblock).
    pub base: Option<ExecutionPayloadBaseV1>,
    /// Delta containing transactions and state changes.
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    /// Associated metadata.
    pub metadata: Metadata,
}

impl Flashblock {
    /// Attempts to decode a flashblock from bytes that may be plain JSON or brotli-compressed JSON.
    pub fn try_decode_message(bytes: impl Into<Bytes>) -> Result<Self, FlashblockDecodeError> {
        let text = Self::try_parse_message(bytes.into())?;

        let payload: FlashblocksPayloadV1 =
            serde_json::from_str(&text).map_err(FlashblockDecodeError::PayloadParse)?;

        let metadata: Metadata = serde_json::from_value(payload.metadata.clone())
            .map_err(FlashblockDecodeError::MetadataParse)?;

        Ok(Self {
            payload_id: payload.payload_id,
            index: payload.index,
            base: payload.base,
            diff: payload.diff,
            metadata,
        })
    }

    fn try_parse_message(bytes: Bytes) -> Result<String, FlashblockDecodeError> {
        if let Ok(text) = std::str::from_utf8(&bytes)
            && text.trim_start().starts_with('{')
        {
            return Ok(text.to_owned());
        }

        let mut decompressor = brotli::Decompressor::new(bytes.as_ref(), 4096);
        let mut decompressed = Vec::new();
        decompressor.read_to_end(&mut decompressed).map_err(FlashblockDecodeError::Decompress)?;

        let text = String::from_utf8(decompressed).map_err(FlashblockDecodeError::Utf8)?;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use alloy_primitives::{Address, B256, Bloom, Bytes as PrimitiveBytes, U256};
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case::plain(encode_plain)]
    #[case::brotli(encode_brotli)]
    fn try_decode_message_handles_plain_and_brotli(
        #[case] encoder: fn(&FlashblocksPayloadV1) -> Bytes,
    ) {
        let payload = sample_payload(json!({
            "block_number": 1234u64
        }));

        let decoded =
            Flashblock::try_decode_message(encoder(&payload)).expect("payload should decode");

        assert_eq!(decoded.payload_id, payload.payload_id);
        assert_eq!(decoded.index, payload.index);
        assert_eq!(decoded.base, payload.base);
        assert_eq!(decoded.diff, payload.diff);
        assert_eq!(decoded.metadata.block_number, 1234);
    }

    #[rstest]
    #[case::plain(encode_plain)]
    #[case::brotli(encode_brotli)]
    fn try_decode_old_format_still_decodes(#[case] encoder: fn(&FlashblocksPayloadV1) -> Bytes) {
        // old format should still decode
        let payload = sample_payload(json!({
            "block_number": 1234u64,
            "receipts": {},
            "new_account_balances": {},
        }));

        let decoded =
            Flashblock::try_decode_message(encoder(&payload)).expect("payload should decode");

        assert_eq!(decoded.payload_id, payload.payload_id);
        assert_eq!(decoded.index, payload.index);
        assert_eq!(decoded.base, payload.base);
        assert_eq!(decoded.diff, payload.diff);
        assert_eq!(decoded.metadata.block_number, 1234);
    }

    #[rstest]
    #[case::invalid_brotli(Bytes::from_static(b"not brotli data"))]
    #[case::missing_metadata(encode_plain(&sample_payload(json!({
    }))))] // missing block_number in metadata
    fn try_decode_message_rejects_invalid_data(#[case] bytes: Bytes) {
        assert!(Flashblock::try_decode_message(bytes).is_err());
    }

    fn encode_plain(payload: &FlashblocksPayloadV1) -> Bytes {
        Bytes::from(serde_json::to_vec(payload).expect("serialize payload"))
    }

    fn encode_brotli(payload: &FlashblocksPayloadV1) -> Bytes {
        let mut compressed = Vec::new();
        let data = serde_json::to_vec(payload).expect("serialize payload");
        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            writer.write_all(&data).expect("write compressed payload");
        }
        Bytes::from(compressed)
    }

    /// Hardcoded JSON representing the v0.4.1 wire format (metadata has only `block_number`).
    /// If deserialization of this string ever breaks, we have introduced a backwards-incompatible
    /// change to the flashblocks websocket schema.
    const V0_4_1_PAYLOAD_JSON: &str = r#"{
        "payload_id": "0x0000000000000000",
        "index": 0,
        "base": {
            "parent_beacon_block_root": "0x0101010101010101010101010101010101010101010101010101010101010101",
            "parent_hash": "0x0202020202020202020202020202020202020202020202020202020202020202",
            "fee_recipient": "0x0000000000000000000000000000000000000000",
            "prev_randao": "0x0303030303030303030303030303030303030303030303030303030303030303",
            "block_number": "0x9",
            "gas_limit": "0xf4240",
            "timestamp": "0x6553f100",
            "extra_data": "0xaabb",
            "base_fee_per_gas": "0xa"
        },
        "diff": {
            "state_root": "0x0404040404040404040404040404040404040404040404040404040404040404",
            "receipts_root": "0x0505050505050505050505050505050505050505050505050505050505050505",
            "logs_bloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "gas_used": "0x7a120",
            "block_hash": "0x0606060606060606060606060606060606060606060606060606060606060606",
            "transactions": ["0x0102"],
            "withdrawals": [],
            "withdrawals_root": "0x0707070707070707070707070707070707070707070707070707070707070707",
            "blob_gas_used": "0x2c"
        },
        "metadata": {
            "block_number": 1234
        }
    }"#;

    /// Hardcoded JSON representing the v0.5.0-rc.3 wire format (metadata includes receipts,
    /// balances, and `access_list`). If deserialization of this string ever breaks, we have
    /// introduced a backwards-incompatible change.
    const V0_5_0_PAYLOAD_JSON: &str = r#"{
        "payload_id": "0x0000000000000000",
        "index": 0,
        "base": {
            "parent_beacon_block_root": "0x0101010101010101010101010101010101010101010101010101010101010101",
            "parent_hash": "0x0202020202020202020202020202020202020202020202020202020202020202",
            "fee_recipient": "0x0000000000000000000000000000000000000000",
            "prev_randao": "0x0303030303030303030303030303030303030303030303030303030303030303",
            "block_number": "0x9",
            "gas_limit": "0xf4240",
            "timestamp": "0x6553f100",
            "extra_data": "0xaabb",
            "base_fee_per_gas": "0xa"
        },
        "diff": {
            "state_root": "0x0404040404040404040404040404040404040404040404040404040404040404",
            "receipts_root": "0x0505050505050505050505050505050505050505050505050505050505050505",
            "logs_bloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "gas_used": "0x7a120",
            "block_hash": "0x0606060606060606060606060606060606060606060606060606060606060606",
            "transactions": ["0x0102"],
            "withdrawals": [],
            "withdrawals_root": "0x0707070707070707070707070707070707070707070707070707070707070707",
            "blob_gas_used": "0x2c"
        },
        "metadata": {
            "receipts": {
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": {
                    "type": "0x2",
                    "status": true,
                    "cumulativeGasUsed": "0x5208",
                    "logs": []
                }
            },
            "new_account_balances": {
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb": "0xde0b6b3a7640000"
            },
            "block_number": 1234,
            "access_list": null
        }
    }"#;

    /// The v0.4.1 wire format must deserialize successfully into current types.
    #[test]
    fn v0_4_1_wire_format_is_backwards_compatible() {
        let payload: FlashblocksPayloadV1 =
            serde_json::from_str(V0_4_1_PAYLOAD_JSON).expect("v0.4.1 JSON must deserialize");

        assert_eq!(payload.index, 0);
        assert!(payload.base.is_some());
        assert_eq!(payload.base.as_ref().unwrap().block_number, 9);
        assert_eq!(payload.diff.gas_used, 500_000);
        assert_eq!(payload.metadata["block_number"], 1234);

        // Client-side Metadata must also parse (ignoring unknown fields)
        let metadata: Metadata =
            serde_json::from_value(payload.metadata).expect("metadata must parse");
        assert_eq!(metadata.block_number, 1234);
    }

    /// The v0.5.0-rc.3 wire format must deserialize successfully into current types.
    #[test]
    fn v0_5_0_wire_format_is_backwards_compatible() {
        let payload: FlashblocksPayloadV1 =
            serde_json::from_str(V0_5_0_PAYLOAD_JSON).expect("v0.5.0 JSON must deserialize");

        assert_eq!(payload.index, 0);
        assert!(payload.base.is_some());
        assert_eq!(payload.diff.gas_used, 500_000);
        assert_eq!(payload.metadata["block_number"], 1234);
        assert!(payload.metadata.get("receipts").is_some());
        assert!(payload.metadata.get("new_account_balances").is_some());
        assert!(payload.metadata.get("access_list").is_some());

        let metadata: Metadata =
            serde_json::from_value(payload.metadata).expect("metadata must parse");
        assert_eq!(metadata.block_number, 1234);
    }

    /// A subsequent flashblock (index > 0) omits `base`. This format must round-trip.
    #[test]
    fn subsequent_flashblock_without_base_deserializes() {
        let json = r#"{
            "payload_id": "0x0000000000000000",
            "index": 3,
            "diff": {
                "state_root": "0x0404040404040404040404040404040404040404040404040404040404040404",
                "receipts_root": "0x0505050505050505050505050505050505050505050505050505050505050505",
                "logs_bloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "gas_used": "0x5208",
                "block_hash": "0x0606060606060606060606060606060606060606060606060606060606060606",
                "transactions": [],
                "withdrawals": [],
                "withdrawals_root": "0x0707070707070707070707070707070707070707070707070707070707070707"
            },
            "metadata": {
                "block_number": 99
            }
        }"#;

        let payload: FlashblocksPayloadV1 =
            serde_json::from_str(json).expect("payload without base must deserialize");
        assert!(payload.base.is_none());
        assert_eq!(payload.index, 3);
    }

    fn sample_payload(metadata: serde_json::Value) -> FlashblocksPayloadV1 {
        FlashblocksPayloadV1 {
            payload_id: PayloadId::default(),
            index: 7,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::from([1u8; 32]),
                parent_hash: B256::from([2u8; 32]),
                fee_recipient: Address::ZERO,
                prev_randao: B256::from([3u8; 32]),
                block_number: 9,
                gas_limit: 1_000_000,
                timestamp: 1_700_000_000,
                extra_data: PrimitiveBytes::from(vec![0xAA, 0xBB]),
                base_fee_per_gas: U256::from(10u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::from([4u8; 32]),
                receipts_root: B256::from([5u8; 32]),
                logs_bloom: Bloom::default(),
                gas_used: 500_000,
                block_hash: B256::from([6u8; 32]),
                transactions: vec![PrimitiveBytes::from(vec![0x01, 0x02])],
                withdrawals: Vec::new(),
                withdrawals_root: B256::from([7u8; 32]),
                blob_gas_used: Some(44),
            },
            metadata,
        }
    }
}
