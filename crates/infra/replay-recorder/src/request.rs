use alloy_primitives::{B256, Bytes, keccak256};
use jsonrpsee_types::TwoPointZero;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// Serialized transaction portion of an insert call.
#[derive(Debug, Deserialize)]
pub struct RecordedTransaction {
    /// Transaction envelope bytes.
    pub raw: Bytes,
}

/// Validated JSON-RPC ID retaining its exact numeric or string representation.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct RpcId(
    /// Original JSON token, including escapes and numeric formatting.
    pub Box<RawValue>,
);

impl<'de> Deserialize<'de> for RpcId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(d)?;
        if matches!(raw.get().bytes().next(), Some(b'"' | b'-' | b'0'..=b'9' | b'n')) {
            Ok(Self(raw))
        } else {
            Err(serde::de::Error::custom("ID must be a string, number, or null"))
        }
    }
}

/// Parsed RPC request retaining the original parameter representation.
#[derive(Debug, Deserialize)]
pub struct RecordedCall {
    /// Required protocol version.
    pub jsonrpc: TwoPointZero,
    /// Missing for notifications; explicit null IDs still receive responses.
    #[serde(default, deserialize_with = "RecordedCall::deserialize_id")]
    pub id: Option<RpcId>,
    /// Arbitrary RPC method name.
    pub method: String,
    /// Original structured params; omission is represented as null.
    #[serde(default, deserialize_with = "RecordedCall::deserialize_params")]
    pub params: Box<RawValue>,
}

impl RecordedCall {
    /// Reject non-structured params as an invalid request object under JSON-RPC §4.2.
    pub fn deserialize_params<'de, D: Deserializer<'de>>(d: D) -> Result<Box<RawValue>, D::Error> {
        let raw = Box::<RawValue>::deserialize(d)?;
        if matches!(raw.get().bytes().next(), Some(b'[' | b'{')) {
            Ok(raw)
        } else {
            Err(serde::de::Error::custom("params must be an array or object"))
        }
    }

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

    use super::RecordedCall;

    #[test]
    fn rejects_present_params_when_not_structured() {
        for params in ["42", "null", "true", r#""text""#] {
            let input = format!(r#"{{"jsonrpc":"2.0","method":"any","params":{params}}}"#);
            assert!(serde_json::from_str::<RecordedCall>(&input).is_err(), "{params}");
        }
    }

    #[test]
    fn preserves_structured_params_when_present_or_omitted() {
        for params in ["[\r\n 1.2300e+02 ]", r#"{ "s": "\u0061" }"#] {
            let input = format!(r#"{{"jsonrpc":"2.0","method":"any","params":{params}}}"#);
            let call: RecordedCall = serde_json::from_str(&input).unwrap();
            assert_eq!(call.params.get().as_bytes(), params.as_bytes());
        }
        let call: RecordedCall =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"any"}"#).unwrap();
        assert_eq!(call.params.get(), "null");
    }

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
    fn preserves_raw_ids_and_distinguishes_notifications() {
        for id in ["-7", "null", "\"text\"", "18446744073709551617", "1.2300e+02", r#""\u0061""#] {
            let call: RecordedCall =
                serde_json::from_str(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"any"}}"#))
                    .unwrap();
            assert_eq!(serde_json::to_string(&call.id.unwrap()).unwrap(), id);
        }
        let notification: RecordedCall =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"any"}"#).unwrap();
        assert!(notification.id.is_none());
        for id in ["true", "[]", "{}"] {
            assert!(
                serde_json::from_str::<RecordedCall>(&format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"any"}}"#
                ))
                .is_err()
            );
        }
    }
}
