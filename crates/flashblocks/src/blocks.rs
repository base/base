//! Contains the [`Flashblock`] and [`Metadata`] types used in Flashblocks.

use std::io::Read;

use alloy_primitives::{Address, B256, U256, map::foldhash::HashMap};
use alloy_rpc_types_engine::PayloadId;
use bytes::Bytes;
use derive_more::{Display, Error};
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Metadata {
    /// Transaction receipts indexed by hash.
    pub receipts: HashMap<B256, OpReceipt>,
    /// Updated account balances.
    pub new_account_balances: HashMap<Address, U256>,
    /// Block number this flashblock belongs to.
    pub block_number: u64,
}

/// A flashblock containing partial block data.
#[derive(Debug, Clone)]
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

/// Errors that can occur while decoding a flashblock payload.
#[derive(Debug, Display, Error)]
pub enum FlashblockDecodeError {
    /// Failed to deserialize the flashblock payload JSON into the expected struct.
    #[display("failed to parse flashblock payload JSON: {_0}")]
    PayloadParse(serde_json::Error),
    /// Failed to deserialize the flashblock metadata into the expected struct.
    #[display("failed to parse flashblock metadata: {_0}")]
    MetadataParse(serde_json::Error),
    /// Brotli decompression failed.
    #[display("failed to decompress brotli payload: {_0}")]
    Decompress(std::io::Error),
    /// The decompressed payload was not valid UTF-8 JSON.
    #[display("decompressed payload is not valid UTF-8 JSON: {_0}")]
    Utf8(std::string::FromUtf8Error),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use alloy_primitives::{Address, Bloom, Bytes as PrimitiveBytes, U256};
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
            "receipts": {},
            "new_account_balances": {},
            "block_number": 1234u64
        }));

        let decoded =
            Flashblock::try_decode_message(encoder(&payload)).expect("payload should decode");

        assert_eq!(decoded.payload_id, payload.payload_id);
        assert_eq!(decoded.index, payload.index);
        assert_eq!(decoded.base, payload.base);
        assert_eq!(decoded.diff, payload.diff);
        assert_eq!(decoded.metadata.block_number, 1234);
        assert!(decoded.metadata.receipts.is_empty());
        assert!(decoded.metadata.new_account_balances.is_empty());
    }

    #[rstest]
    #[case::invalid_brotli(Bytes::from_static(b"not brotli data"))]
    #[case::missing_metadata(encode_plain(&sample_payload(json!({
        "receipts": {},
        "new_account_balances": {}
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
