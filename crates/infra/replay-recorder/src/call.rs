use std::{io, net::SocketAddr};

use alloy_primitives::{B256, Bytes, keccak256};
use base_replay::{ReplayRecord, ReplayStream};
use jsonrpsee_types::TwoPointZero;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// Metadata assigned at the ingress boundary.
#[derive(Debug)]
pub struct CaptureContext {
    /// Ingress timestamp from the caller's clock, in Unix milliseconds.
    pub received_at_ms: u64,
    /// Remote socket address including port.
    pub source: SocketAddr,
    /// Session-local sequence number used only for batches.
    pub batch_id: u64,
}

/// Incoming single call or ordered JSON-RPC batch.
#[derive(Debug)]
pub enum RpcEnvelope {
    /// One call.
    Single(RecordedCall),
    /// Calls sharing a recorder-assigned batch ID.
    Batch(Vec<RecordedCall>),
}

impl RpcEnvelope {
    /// Parse without Serde's untagged buffering, preserving raw params for JSONL.
    pub fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: &RawValue = serde_json::from_slice(bytes)?;
        let envelope = if raw.get().starts_with('[') {
            Self::Batch(serde_json::from_str(raw.get())?)
        } else {
            Self::Single(serde_json::from_str(raw.get())?)
        };
        let calls = match &envelope {
            Self::Single(call) => std::slice::from_ref(call),
            Self::Batch(calls) => calls.as_slice(),
        };
        if calls.is_empty() || calls.iter().any(|call| call.params.get().contains(['\r', '\n'])) {
            return Err(serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty batch or multiline params",
            )));
        }
        Ok(envelope)
    }

    /// Convert calls in wire order, retaining IDs for response construction.
    pub fn into_records(self, context: &CaptureContext) -> Vec<(Option<RpcId>, ReplayRecord)> {
        let (calls, batch_id) = match self {
            Self::Single(call) => (vec![call], None),
            Self::Batch(calls) => (calls, Some(context.batch_id)),
        };
        calls
            .into_iter()
            .map(|call| {
                let record = ReplayRecord {
                    stream: ReplayStream::BuilderIngress,
                    received_at_ms: context.received_at_ms,
                    tx_hash: call.tx_hash(),
                    method: call.method,
                    params: call.params,
                    batch_id,
                    source: context.source.to_string(),
                };
                (call.id, record)
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::{CaptureContext, RecordedCall, RpcEnvelope};

    #[test]
    fn derives_hash_when_insert_identifies_one_transaction() {
        for (method, params, expected) in [
            ("base_insertValidatedTransaction", r#"[{"raw":"0x0102"}]"#, Some(keccak256([1, 2]))),
            (
                "base_insertValidatedTransactions",
                r#"[[{"raw":"0x0102"}]]"#,
                Some(keccak256([1, 2])),
            ),
            ("base_insertValidatedTransactions", r#"[[{"raw":"0x01"},{"raw":"0x02"}]]"#, None),
            ("base_insertValidatedTransaction", r#"[{"raw":"invalid"}]"#, None),
            ("unknown", r#"[{"raw":"0x0102"}]"#, None),
        ] {
            let call: RecordedCall = serde_json::from_str(&format!(
                r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#
            ))
            .unwrap();
            assert_eq!(call.tx_hash(), expected);
        }
    }

    #[test]
    fn preserves_id_when_signed_or_null_and_distinguishes_notifications() {
        for id in ["-7", "null", "\"text\""] {
            let call: RecordedCall =
                serde_json::from_str(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"any"}}"#))
                    .unwrap();
            assert_eq!(serde_json::to_string(&call.id.unwrap()).unwrap(), id);
            assert_eq!(call.params.get(), "null");
        }
        let notification: RecordedCall =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"any"}"#).unwrap();
        assert!(notification.id.is_none());
    }

    #[test]
    fn converts_single_and_batch_without_changing_params() {
        let params = r#"[ { "n" : 1.2300e+02, "s": "\u0061" } ]"#;
        let call = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"future","params":{params}}}"#);
        let context = CaptureContext {
            received_at_ms: 123,
            source: "127.0.0.1:4321".parse().unwrap(),
            batch_id: 42,
        };
        for (input, expected) in
            [(call.clone(), vec![None]), (format!("[{call},{call}]"), vec![Some(42), Some(42)])]
        {
            let records = RpcEnvelope::parse(input.as_bytes()).unwrap().into_records(&context);
            assert_eq!(records.iter().map(|(_, r)| r.batch_id).collect::<Vec<_>>(), expected);
            for (id, record) in records {
                assert_eq!(serde_json::to_string(&id.unwrap()).unwrap(), "7");
                assert_eq!(record.method, "future");
                assert_eq!(record.received_at_ms, 123);
                assert_eq!(record.source, "127.0.0.1:4321");
                assert_eq!(record.params.get().as_bytes(), params.as_bytes());
                assert!(record.to_jsonl_line().unwrap().contains(params));
            }
        }
        for invalid in
            ["[]", "not json", "{\"jsonrpc\":\"2.0\",\"method\":\"bad\",\"params\":[\r\n1]}"]
        {
            assert!(RpcEnvelope::parse(invalid.as_bytes()).is_err());
        }
    }
}
