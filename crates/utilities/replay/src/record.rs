use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// External format identifier; readers select this schema from transport or dataset metadata.
pub const REPLAY_RECORD_FORMAT: &str = "replay-record/v1";

/// Ingress boundary at which a call was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayStream {
    /// Calls received at a builder RPC endpoint.
    BuilderIngress,
    /// Reserved for future mempool ingress recording.
    MempoolIngress,
}

/// One JSON-RPC call in a [`REPLAY_RECORD_FORMAT`] dataset.
///
/// Use [`Self::to_jsonl_line`] for checked JSONL framing. Batch calls retain order and share a batch ID.
/// The enclosing transport or dataset metadata must identify v1 before deserialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayRecord {
    /// Ingress boundary that received this call.
    pub stream: ReplayStream,
    /// Recorder ingress timestamp, in milliseconds since the Unix epoch.
    pub received_at_ms: u64,
    /// JSON-RPC method to resend.
    pub method: String,
    /// Original JSON parameter value, preserving internal whitespace and number formatting.
    pub params: Box<RawValue>,
    /// Recorder-assigned batch sequence number, scoped to this capture session, not an RPC ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<u64>,
    /// Remote socket address, including port, as observed by the recorder.
    pub source: String,
    /// Transaction hash, when this call is attributable to one transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<B256>,
}

/// Failure to serialize a replay record as one JSONL line.
#[derive(Debug, thiserror::Error)]
pub enum ReplayRecordError {
    /// Raw parameters contain a literal LF or CR, which would break JSONL framing.
    #[error("raw params contain a literal LF or CR")]
    MultilineParams,
    /// JSON serialization failed.
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}

impl ReplayRecord {
    /// Accept the current writer schema and legacy v1 exports.
    pub fn supports_format(format: &str) -> bool {
        format == REPLAY_RECORD_FORMAT || format == "replay-record/v1"
    }

    /// Decode a JSONL record using the framing owned by this crate.
    pub fn from_jsonl_line(line: &str) -> Result<Self, ReplayRecordError> {
        Ok(serde_json::from_str(line)?)
    }

    /// Serialize one physical JSONL line, preserving raw params and appending a newline.
    ///
    /// # Errors
    /// Returns [`ReplayRecordError::MultilineParams`] for literal LF or CR in raw params,
    /// or [`ReplayRecordError::Serialization`] if JSON serialization fails.
    pub fn to_jsonl_line(&self) -> Result<String, ReplayRecordError> {
        let mut line = serde_json::to_string(self)?;
        if line.contains(['\n', '\r']) {
            return Err(ReplayRecordError::MultilineParams);
        }
        line.push('\n');
        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use serde::Deserialize;
    use serde_json::{Value, value::RawValue};

    use super::{ReplayRecord, ReplayRecordError, ReplayStream};

    #[derive(Debug, Deserialize)]
    struct CapturedCall {
        method: String,
        params: Box<RawValue>,
    }

    #[test]
    fn public_decoder_reads_writer_format_and_retains_legacy_support() {
        let input = r#"{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[1.2300],"source":"test"}"#;
        let record: ReplayRecord = serde_json::from_str(input).unwrap();

        let decoded = ReplayRecord::from_jsonl_line(&record.to_jsonl_line().unwrap()).unwrap();

        assert_eq!(decoded.params.get(), "[1.2300]");
        assert!(ReplayRecord::supports_format(super::REPLAY_RECORD_FORMAT));
        assert!(ReplayRecord::supports_format("replay-record/v1"));
        assert!(!ReplayRecord::supports_format("replay-record/unsupported"));
    }

    #[test]
    fn captured_forwarder_call_survives_jsonl_round_trip() {
        let calls: Vec<CapturedCall> =
            serde_json::from_str(include_str!("../testdata/insert-validated-transaction.json"))
                .unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        let record = ReplayRecord {
            stream: ReplayStream::BuilderIngress,
            received_at_ms: 1_700_000_000_123,
            method: call.method.clone(),
            params: call.params.clone(),
            batch_id: Some(0),
            source: "127.0.0.1:43123".into(),
            tx_hash: None,
        };

        let line = record.to_jsonl_line().unwrap();
        let decoded: ReplayRecord = serde_json::from_str(line.trim_end()).unwrap();

        assert_eq!(line.lines().count(), 1);
        assert_eq!(decoded.method, "base_insertValidatedTransaction");
        assert_eq!(decoded.params.get().as_bytes(), call.params.get().as_bytes());
        let params: Vec<Value> = serde_json::from_str(decoded.params.get()).unwrap();
        assert_eq!(params.len(), 1);
        assert!(params[0]["raw"].as_str().unwrap().starts_with("0x02"));
    }

    #[test]
    fn params_survive_round_trip_when_formatting_is_noncanonical() {
        let params = r#"[ { "amount" : 1.2300e+02, "escaped": "\u0061", "n": -0 } ]"#;
        let record = ReplayRecord {
            stream: ReplayStream::BuilderIngress,
            received_at_ms: 1_700_000_000_123,
            method: "base_insertValidatedTransaction".into(),
            params: RawValue::from_string(params.into()).unwrap(),
            batch_id: Some(42),
            source: "127.0.0.1:43123".into(),
            tx_hash: Some(B256::repeat_byte(0xab)),
        };

        let encoded = record.to_jsonl_line().unwrap();
        let decoded: ReplayRecord = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.params.get().as_bytes(), params.as_bytes());
        assert!(encoded.contains(params));
        assert_eq!(decoded.stream, record.stream);
        assert_eq!(decoded.received_at_ms, record.received_at_ms);
        assert_eq!(decoded.method, record.method);
        assert_eq!(decoded.batch_id, record.batch_id);
        assert_eq!(decoded.source, record.source);
        assert_eq!(decoded.tx_hash, record.tx_hash);
    }

    #[test]
    fn jsonl_rejects_params_when_line_breaks_are_literal() {
        for separator in ["\n", "\r", "\r\n"] {
            let input = format!(
                r#"{{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[{separator}1{separator}],"source":"[::1]:1234"}}"#
            );
            let record: ReplayRecord = serde_json::from_str(&input).unwrap();

            let result = record.to_jsonl_line();

            assert!(matches!(result, Err(ReplayRecordError::MultilineParams)));
        }
    }

    #[test]
    fn jsonl_preserves_params_when_line_breaks_are_escaped() {
        let params = r#"[ "line\nbreak", "carriage\rreturn" ]"#;
        let input = format!(
            r#"{{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":{params},"source":"[::1]:1234"}}"#
        );
        let record: ReplayRecord = serde_json::from_str(&input).unwrap();

        let line = record.to_jsonl_line().unwrap();

        assert!(line.ends_with('\n'));
        assert!(!line[..line.len() - 1].contains(['\n', '\r']));
        let decoded: ReplayRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded.params.get().as_bytes(), params.as_bytes());
    }

    #[test]
    fn optional_fields_are_omitted_when_absent() {
        let input = r#"{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[],"source":"[::1]:1234"}"#;
        let record: ReplayRecord = serde_json::from_str(input).unwrap();

        let encoded = serde_json::to_value(&record).unwrap();

        assert!(encoded.get("batch_id").is_none());
        assert!(encoded.get("tx_hash").is_none());
        assert!(record.batch_id.is_none());
        assert!(record.tx_hash.is_none());
    }

    #[test]
    fn stream_uses_snake_case_when_serialized() {
        for (stream, wire) in [
            (ReplayStream::BuilderIngress, "builder_ingress"),
            (ReplayStream::MempoolIngress, "mempool_ingress"),
        ] {
            let encoded = serde_json::to_value(stream).unwrap();

            assert_eq!(encoded, Value::String(wire.into()));
            assert_eq!(serde_json::from_value::<ReplayStream>(encoded).unwrap(), stream);
        }
    }

    #[test]
    fn record_is_rejected_when_stream_is_unknown() {
        let input = r#"{"stream":"future_ingress","received_at_ms":0,"method":"call","params":[],"source":"127.0.0.1:1234"}"#;

        let result = serde_json::from_str::<ReplayRecord>(input);

        assert!(result.unwrap_err().to_string().contains("unknown variant"));
    }
}
