//! Native block stream message type.

use alloc::vec::Vec;

use alloy_primitives::Bytes;
use base_common_consensus::BaseReceipt;
use serde::{Deserialize, Serialize};

use crate::BaseHeaderResponse;

/// One sealed block broadcast on the builder block stream WebSocket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockStreamPayloadV1 {
    /// Block number as a plain integer, used as the stream resume key.
    pub block_number: u64,
    /// RPC-format header, including `hash` and `timestampMs`.
    pub header: BaseHeaderResponse,
    /// EIP-2718 encoded transactions in block order.
    pub transactions: Vec<Bytes>,
    /// Consensus receipts in block order.
    pub receipts: Vec<BaseReceipt>,
}

impl BlockStreamPayloadV1 {
    /// Largest encoded message the stream carries, in bytes.
    ///
    /// Messages go out as a single unfragmented WebSocket frame, so this matches tungstenite's
    /// default maximum frame size and stream clients work without raising their own limits.
    pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeSet, vec};

    use alloy_consensus::{Header as ConsensusHeader, Receipt};
    use alloy_primitives::{Address, B256, Log, LogData};
    use alloy_rpc_types_eth::Header;
    use serde_json::json;

    use super::*;

    /// Pin the JSON wire format of [`BlockStreamPayloadV1`].
    ///
    /// Changing field names or encodings breaks downstream stream consumers.
    #[test]
    fn json_format_is_stable() {
        let log = Log {
            address: Address::repeat_byte(0xAA),
            data: LogData::new_unchecked(vec![B256::repeat_byte(0xBB)], Bytes::new()),
        };
        let payload = BlockStreamPayloadV1 {
            block_number: 42,
            header: BaseHeaderResponse::with_timestamp_ms(
                Header::new(ConsensusHeader { number: 42, timestamp: 10, ..Default::default() }),
                Some(10_200),
            ),
            transactions: vec![Bytes::from(vec![0x02, 0x01])],
            receipts: vec![BaseReceipt::Eip1559(Receipt {
                status: true.into(),
                cumulative_gas_used: 21_000,
                logs: vec![log],
            })],
        };

        let value = serde_json::to_value(&payload).unwrap();
        let keys: BTreeSet<&str> = value.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, ["blockNumber", "header", "receipts", "transactions"].into());

        assert_eq!(value["blockNumber"], json!(42));
        assert_eq!(value["header"]["number"], json!("0x2a"));
        assert_eq!(value["header"]["timestampMs"], json!("0x27d8"));
        assert_eq!(value["transactions"], json!(["0x0201"]));
        assert_eq!(value["receipts"][0]["type"], json!("0x2"));
        assert_eq!(
            value["receipts"][0]["logs"][0]["address"],
            json!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );

        let decoded: BlockStreamPayloadV1 = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, payload);
    }
}
