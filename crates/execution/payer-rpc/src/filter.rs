//! Local checks applied before a payer request is forwarded.

use std::str::FromStr;

use alloy_primitives::{Address, hex};
use serde_json::{Map, Value};
use thiserror::Error;

/// Rejects payer requests that should not be forwarded.
#[derive(Debug)]
pub struct PayerRequestFilter;

impl PayerRequestFilter {
    /// Maximum JSON size of a payer request, in bytes.
    pub const MAX_REQUEST_BYTES: usize = 128 * 1024;

    /// Maximum decoded `payer_sendTransaction` payload, in bytes.
    pub const MAX_TRANSACTION_BYTES: usize = 128 * 1024;

    /// Maximum number of `calls` or `frames` entries.
    pub const MAX_INTENT_LEN: usize = 64;

    /// Checks a `payer_getTerms` body against `chain_id`.
    pub fn validate_get_terms(request: &Value, chain_id: u64) -> Result<(), PayerRequestError> {
        let object = Self::object(request)?;
        let request_chain_id = object
            .get("chainId")
            .and_then(Self::parse_chain_id)
            .ok_or(PayerRequestError::MissingChainId)?;
        if request_chain_id != chain_id {
            return Err(PayerRequestError::ChainIdMismatch);
        }
        let from =
            object.get("from").and_then(Value::as_str).ok_or(PayerRequestError::MissingFrom)?;
        if Address::from_str(from).is_err() {
            return Err(PayerRequestError::MissingFrom);
        }

        match (Self::intent_field(object.get("calls"))?, Self::intent_field(object.get("frames"))?)
        {
            (IntentField::Array(calls), IntentField::Absent) => Self::validate_calls(calls),
            (IntentField::Absent, IntentField::Array(frames)) => Self::validate_frames(frames),
            _ => Err(PayerRequestError::IntentShape),
        }
    }

    /// Checks a `payer_sendTransaction` body.
    pub fn validate_send_transaction(request: &Value) -> Result<(), PayerRequestError> {
        let object = Self::object(request)?;
        let transaction = object
            .get("transaction")
            .and_then(Value::as_str)
            .ok_or(PayerRequestError::InvalidTransaction)?;
        let bytes = hex::decode(transaction).map_err(|_| PayerRequestError::InvalidTransaction)?;
        if bytes.is_empty() || bytes.len() > Self::MAX_TRANSACTION_BYTES {
            return Err(PayerRequestError::InvalidTransaction);
        }
        if let Some(context) = object.get("context")
            && !context.is_null()
            && !context.is_object()
        {
            return Err(PayerRequestError::InvalidContext);
        }
        Ok(())
    }

    fn object(request: &Value) -> Result<&Map<String, Value>, PayerRequestError> {
        let size = serde_json::to_vec(request).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
        if size > Self::MAX_REQUEST_BYTES {
            return Err(PayerRequestError::TooLarge);
        }
        request.as_object().ok_or(PayerRequestError::NotAnObject)
    }

    fn parse_chain_id(value: &Value) -> Option<u64> {
        let raw = value.as_str()?;
        let hex_digits = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X"))?;
        if hex_digits.is_empty() || !hex_digits.chars().all(|digit| digit.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(hex_digits, 16).ok()
    }

    const fn intent_field(value: Option<&Value>) -> Result<IntentField<'_>, PayerRequestError> {
        match value {
            None | Some(Value::Null) => Ok(IntentField::Absent),
            Some(Value::Array(items)) => Ok(IntentField::Array(items)),
            Some(_) => Err(PayerRequestError::IntentShape),
        }
    }

    fn validate_calls(calls: &[Value]) -> Result<(), PayerRequestError> {
        if calls.is_empty() || calls.len() > Self::MAX_INTENT_LEN {
            return Err(PayerRequestError::InvalidCalls);
        }
        if calls.iter().any(|call| call.get("to").and_then(Value::as_str).is_none()) {
            return Err(PayerRequestError::InvalidCalls);
        }
        Ok(())
    }

    const fn validate_frames(frames: &[Value]) -> Result<(), PayerRequestError> {
        if frames.is_empty() || frames.len() > Self::MAX_INTENT_LEN {
            return Err(PayerRequestError::InvalidFrames);
        }
        Ok(())
    }
}

enum IntentField<'a> {
    Absent,
    Array(&'a Vec<Value>),
}

/// A payer request rejected before it is forwarded.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PayerRequestError {
    /// The JSON body exceeds [`PayerRequestFilter::MAX_REQUEST_BYTES`].
    #[error("payer request exceeds the size limit")]
    TooLarge,
    /// The parameter is not a JSON object.
    #[error("payer request must be an object")]
    NotAnObject,
    /// `payer_getTerms` is missing `chainId`.
    #[error("payer_getTerms requires chainId")]
    MissingChainId,
    /// `chainId` is not this node's chain.
    #[error("payer request chainId does not match this node")]
    ChainIdMismatch,
    /// `payer_getTerms` is missing a sender address.
    #[error("payer_getTerms requires from")]
    MissingFrom,
    /// `payer_getTerms` does not carry exactly one of `calls` or `frames`.
    #[error("payer_getTerms requires exactly one of calls or frames")]
    IntentShape,
    /// `calls` is empty, too long, or missing `to`.
    #[error("payer_getTerms calls must be a non-empty list of call objects")]
    InvalidCalls,
    /// `frames` is empty or too long.
    #[error("payer_getTerms frames must be a non-empty list")]
    InvalidFrames,
    /// `transaction` is missing, empty, or larger than the size cap.
    #[error("payer_sendTransaction requires a hex-encoded transaction")]
    InvalidTransaction,
    /// `context` is present and is not an object.
    #[error("payer request context must be an object")]
    InvalidContext,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CHAIN_ID: u64 = 8453;

    fn terms() -> Value {
        json!({
            "chainId": "0x2105",
            "from": "0x0000000000000000000000000000000000000001",
            "calls": [{ "to": "0x0000000000000000000000000000000000000002" }]
        })
    }

    #[test]
    fn get_terms_accepts_a_call_intent() {
        assert!(PayerRequestFilter::validate_get_terms(&terms(), CHAIN_ID).is_ok());
    }

    #[test]
    fn get_terms_rejects_a_different_chain() {
        let mut request = terms();
        request["chainId"] = json!("0x14a34");

        assert_eq!(
            PayerRequestFilter::validate_get_terms(&request, CHAIN_ID),
            Err(PayerRequestError::ChainIdMismatch)
        );
    }

    #[test]
    fn get_terms_requires_exactly_one_intent() {
        let mut both = terms();
        both["frames"] = json!([{ "mode": "0x2" }]);
        assert_eq!(
            PayerRequestFilter::validate_get_terms(&both, CHAIN_ID),
            Err(PayerRequestError::IntentShape)
        );

        let mut neither = terms();
        neither.as_object_mut().unwrap().remove("calls");
        assert_eq!(
            PayerRequestFilter::validate_get_terms(&neither, CHAIN_ID),
            Err(PayerRequestError::IntentShape)
        );
    }

    #[test]
    fn send_transaction_rejects_empty_and_accepts_a_payload() {
        assert_eq!(
            PayerRequestFilter::validate_send_transaction(&json!({ "transaction": "0x" })),
            Err(PayerRequestError::InvalidTransaction)
        );
        assert!(
            PayerRequestFilter::validate_send_transaction(&json!({
                "transaction": "0x02",
                "context": { "appId": "demo" }
            }))
            .is_ok()
        );
    }
}
