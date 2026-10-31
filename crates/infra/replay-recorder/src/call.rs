use alloy_primitives::{B256, Bytes, keccak256};
use jsonrpsee_types::TwoPointZero;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// Incoming single call or ordered JSON-RPC batch.
#[derive(Debug)]
pub enum RpcEnvelope {
    /// One call.
    Single(RecordedCall),
    /// Calls sharing a recorder-assigned batch ID.
    Batch(Vec<RecordedCall>),
}

impl RpcEnvelope {
    /// Parse without Serde's untagged buffering, which discards raw JSON formatting.
    pub fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: &RawValue = serde_json::from_slice(bytes)?;
        if raw.get().starts_with('[') {
            serde_json::from_str(raw.get()).map(Self::Batch)
        } else {
            serde_json::from_str(raw.get()).map(Self::Single)
        }
    }
}

/// Serialized transaction portion of an insert call.
#[derive(Debug, Deserialize)]
pub struct RecordedTransaction {
    /// Transaction envelope bytes.
    pub raw: Bytes,
}

/// JSON-RPC ID, including signed numeric IDs and explicit null.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RpcId {
    /// Numeric correlation ID.
    Number(serde_json::Number),
    /// String correlation ID.
    String(String),
    /// Explicit null ID.
    Null,
}

/// Parsed RPC envelope retaining the original parameter representation.
#[derive(Debug, Deserialize)]
pub struct RecordedCall {
    /// Required protocol version.
    pub jsonrpc: TwoPointZero,
    /// Missing for notifications; explicit null IDs still receive responses.
    #[serde(default, deserialize_with = "RecordedCall::deserialize_id")]
    pub id: Option<RpcId>,
    /// Arbitrary RPC method name.
    pub method: String,
    /// Original params; omitted parameters are represented as null.
    #[serde(default)]
    pub params: Box<RawValue>,
}

impl RecordedCall {
    /// Preserve the distinction between missing and explicit null IDs.
    pub fn deserialize_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RpcId>, D::Error> {
        RpcId::deserialize(d).map(Some)
    }

    /// Derive a hash only when the call identifies one serialized transaction.
    pub fn tx_hash(&self) -> Option<B256> {
        let raw = match self.method.as_str() {
            "base_insertValidatedTransaction" => {
                let [tx] =
                    serde_json::from_str::<[RecordedTransaction; 1]>(self.params.get()).ok()?;
                tx.raw
            }
            "base_insertValidatedTransactions" => {
                let [mut txs] =
                    serde_json::from_str::<[Vec<RecordedTransaction>; 1]>(self.params.get())
                        .ok()?;
                if txs.len() != 1 {
                    return None;
                }
                txs.pop()?.raw
            }
            "eth_sendRawTransaction" => {
                let [raw] = serde_json::from_str::<[Bytes; 1]>(self.params.get()).ok()?;
                raw
            }
            _ => return None,
        };
        (!raw.is_empty()).then(|| keccak256(raw))
    }
}
