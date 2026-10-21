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

    /// A `TEMPORARILY_UNAVAILABLE` rejection with `reason`, which wallets retry shortly.
    pub fn temporarily_unavailable(reason: impl ToString) -> ErrorObjectOwned {
        Self::error(json!({ "code": "TEMPORARILY_UNAVAILABLE", "reason": reason.to_string() }))
    }

    /// Returns whether the payer did not answer `error`: the upstream could not be reached, or
    /// something in front of the payer, such as a gateway, answered in its place.
    ///
    /// A payer answers with `PAYER_REJECTED` or a standard request error (`-32601`, `-32602`),
    /// so any other error did not come from it.
    pub fn is_unanswered(error: &SequencerClientError) -> bool {
        let SequencerClientError::HttpError(inner) = error;
        inner.as_error_resp().is_none_or(|payload| {
            let rejected =
                payload.code == i64::from(Self::CODE) && payload.message == Self::MESSAGE;
            let request_error = [ErrorCode::MethodNotFound, ErrorCode::InvalidParams]
                .iter()
                .any(|code| payload.code == i64::from(code.code()));
            !rejected && !request_error
        })
    }

    fn error(data: Value) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(Self::CODE, Self::MESSAGE, Some(data))
    }
}
