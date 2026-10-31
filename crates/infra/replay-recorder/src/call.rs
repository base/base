use std::{io, net::SocketAddr};

use base_replay::{ReplayRecord, ReplayStream};
use serde_json::value::RawValue;

use crate::{RecordedCall, RpcId};

/// A valid call and response ID, or one invalid JSON-RPC batch member.
pub type CaptureResult = Result<(Option<RpcId>, ReplayRecord), serde_json::Error>;

/// Metadata assigned at the ingress boundary.
#[derive(Debug)]
pub struct CaptureContext {
    /// Arrival timestamp from the caller's clock, in Unix milliseconds.
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
    Batch(Vec<Result<RecordedCall, serde_json::Error>>),
}

impl RpcEnvelope {
    /// Parse envelopes while preserving raw params.
    pub fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: &RawValue = serde_json::from_slice(bytes)?;
        let envelope = if raw.get().starts_with('[') {
            let members: Vec<&RawValue> = serde_json::from_str(raw.get())?;
            Self::Batch(
                members.into_iter().map(|member| serde_json::from_str(member.get())).collect(),
            )
        } else {
            Self::Single(serde_json::from_str(raw.get())?)
        };
        if matches!(&envelope, Self::Batch(calls) if calls.is_empty()) {
            return Err(serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty batch",
            )));
        }
        Ok(envelope)
    }

    /// Convert calls in wire order, retaining IDs for response construction.
    pub fn into_records(self, context: &CaptureContext) -> Vec<CaptureResult> {
        let (calls, batch_id) = match self {
            Self::Single(call) => (vec![Ok(call)], None),
            Self::Batch(calls) => (calls, Some(context.batch_id)),
        };
        calls
            .into_iter()
            .map(|call| {
                let call = call?;
                let record = ReplayRecord {
                    stream: ReplayStream::BuilderIngress,
                    received_at_ms: context.received_at_ms,
                    tx_hash: call.tx_hash(),
                    method: call.method,
                    params: call.params,
                    batch_id,
                    source: context.source.to_string(),
                };
                Ok((call.id, record))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use base_replay::ReplayRecord;

    use super::{CaptureContext, RpcEnvelope};

    #[test]
    fn converts_single_and_batch_without_changing_multiline_params() {
        let params = "[\r\n { \"n\" : 1.2300e+02, \"s\": \"\\u0061\" }\n]";
        let call = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"future","params":{params}}}"#);
        let context = CaptureContext {
            received_at_ms: 123,
            source: "127.0.0.1:4321".parse().unwrap(),
            batch_id: 42,
        };
        for (input, expected) in
            [(call.clone(), vec![None]), (format!("[{call},{call}]"), vec![Some(42), Some(42)])]
        {
            let records: Vec<_> = RpcEnvelope::parse(input.as_bytes())
                .unwrap()
                .into_records(&context)
                .into_iter()
                .map(Result::unwrap)
                .collect();
            assert_eq!(records.iter().map(|(_, r)| r.batch_id).collect::<Vec<_>>(), expected);
            for (id, record) in records {
                assert_eq!(serde_json::to_string(&id.unwrap()).unwrap(), "7");
                assert_eq!(record.received_at_ms, 123);
                assert_eq!(record.source, "127.0.0.1:4321");
                assert_eq!(record.params.get().as_bytes(), params.as_bytes());
                let restored =
                    ReplayRecord::from_jsonl_line(&record.to_jsonl_line().unwrap()).unwrap();
                assert_eq!(restored.params.get().as_bytes(), params.as_bytes());
            }
        }
        assert!(RpcEnvelope::parse(b"[]").is_err());
    }

    #[test]
    fn valid_members_survive_invalid_batch_members() {
        let context = CaptureContext {
            received_at_ms: 123,
            source: "127.0.0.1:4321".parse().unwrap(),
            batch_id: 42,
        };
        let results = RpcEnvelope::parse(br#"[{"jsonrpc":"2.0","id":1,"method":"ok"},1]"#)
            .unwrap()
            .into_records(&context);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().1.method, "ok");
        assert!(results[1].is_err());
    }
}
