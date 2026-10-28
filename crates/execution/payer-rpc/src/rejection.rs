//! ERC-8168 `PAYER_REJECTED` errors the node answers with itself.

use base_execution_rpc::SequencerClientError;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};
use serde_json::{Value, json};

/// Builds the `PAYER_REJECTED` errors a payer returns, for payer requests the node answers
/// itself.
#[derive(Debug)]
pub struct PayerRejection;

impl PayerRejection {
    /// JSON-RPC code of a payer rejection.
    pub const CODE: i32 = -32000;

    /// JSON-RPC message of a payer rejection.
    pub const MESSAGE: &str = "PAYER_REJECTED";

    /// An `INVALID_TRANSACTION` rejection with `reason`.
    pub fn invalid_transaction(reason: impl ToString) -> ErrorObjectOwned {
        Self::error(json!({ "code": "INVALID_TRANSACTION", "reason": reason.to_string() }))
    }

    /// A `TEMPORARILY_UNAVAILABLE` rejection with `reason`, which wallets retry shortly.
    pub fn temporarily_unavailable(reason: impl ToString) -> ErrorObjectOwned {
        Self::error(json!({ "code": "TEMPORARILY_UNAVAILABLE", "reason": reason.to_string() }))
    }

    /// The payer's own error in `error`, or `None` when the payer did not answer: the upstream
    /// could not be reached, or something in front of the payer, such as a gateway, answered in
    /// its place.
    ///
    /// A payer answers with `PAYER_REJECTED` or a standard request error (`-32601`, `-32602`),
    /// so any other error did not come from it. The error is also read from the body of a
    /// non-2xx reply, which the HTTP transport leaves unparsed.
    pub fn from_upstream(error: &SequencerClientError) -> Option<ErrorObjectOwned> {
        let SequencerClientError::HttpError(inner) = error;
        let answer = match inner.as_error_resp() {
            Some(payload) => ErrorObjectOwned::owned(
                i32::try_from(payload.code).ok()?,
                payload.message.to_string(),
                payload.data.clone(),
            ),
            None => {
                let body = &inner.as_transport_err()?.as_http_error()?.body;
                let body: Value = serde_json::from_str(body).ok()?;
                let error = body.get("error")?;
                ErrorObjectOwned::owned(
                    i32::try_from(error.get("code")?.as_i64()?).ok()?,
                    error.get("message")?.as_str()?.to_owned(),
                    error.get("data").cloned(),
                )
            }
        };
        let rejected = answer.code() == Self::CODE && answer.message() == Self::MESSAGE;
        let request_error = [ErrorCode::MethodNotFound, ErrorCode::InvalidParams]
            .iter()
            .any(|code| answer.code() == code.code());
        (rejected || request_error).then_some(answer)
    }

    fn error(data: Value) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(Self::CODE, Self::MESSAGE, Some(data))
    }
}
