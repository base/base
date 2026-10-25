//! ERC-8168 `PAYER_REJECTED` errors the node answers with itself.

use alloy_rpc_types_eth::error::EthRpcErrorCode;
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

    /// The rejection for a phase-0 simulation that failed with `error`: `EXECUTION_REVERTED`
    /// for a revert, carrying its data, and `INVALID_TRANSACTION` for an EIP-8130 rejection
    /// (`-32003`), such as an account change that fails to apply. A failed call, including one
    /// that runs out of gas or moves value the sender lacks, reverts the phase, so it arrives
    /// as a revert.
    ///
    /// Returns `None` for any other error, which says nothing reliable about the transaction.
    /// reth shares `-32000` between request checks and node failures such as execution
    /// timeouts, and the validity-window check (`-32602`) runs on the latest block's clock,
    /// which trails the pool's.
    pub fn for_phase_zero(error: &ErrorObjectOwned) -> Option<ErrorObjectOwned> {
        let code = error.code();
        if code == EthRpcErrorCode::ExecutionError.code() {
            let mut revert = json!({ "phase": 0 });
            if let Some(data) =
                error.data().and_then(|data| serde_json::from_str::<Value>(data.get()).ok())
            {
                revert["data"] = data;
            }
            return Some(Self::error(json!({
                "code": "EXECUTION_REVERTED",
                "reason": error.message(),
                "revert": revert,
            })));
        }
        (code == EthRpcErrorCode::TransactionRejected.code())
            .then(|| Self::invalid_transaction(error.message()))
    }

    fn error(data: Value) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(Self::CODE, Self::MESSAGE, Some(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(error: &ErrorObjectOwned) -> Value {
        serde_json::from_str(error.data().unwrap().get()).unwrap()
    }

    #[test]
    fn reports_a_phase_zero_revert_with_its_data() {
        let revert = ErrorObjectOwned::owned(
            EthRpcErrorCode::ExecutionError.code(),
            "execution reverted",
            Some("0x08c379a0"),
        );

        let rejection = PayerRejection::for_phase_zero(&revert).unwrap();

        assert_eq!(rejection.code(), PayerRejection::CODE);
        assert_eq!(rejection.message(), PayerRejection::MESSAGE);
        assert_eq!(
            data(&rejection),
            json!({
                "code": "EXECUTION_REVERTED",
                "reason": "execution reverted",
                "revert": { "phase": 0, "data": "0x08c379a0" }
            })
        );
    }

    #[test]
    fn reports_a_refused_simulation_as_an_invalid_transaction() {
        let reason = "EIP-8130 transaction rejected: unsupported account change type";
        let refused = ErrorObjectOwned::owned(
            EthRpcErrorCode::TransactionRejected.code(),
            reason,
            None::<()>,
        );

        let rejection = PayerRejection::for_phase_zero(&refused).unwrap();

        assert_eq!(data(&rejection), json!({ "code": "INVALID_TRANSACTION", "reason": reason }));
    }

    #[test]
    fn passes_a_simulation_that_says_nothing_about_the_transaction() {
        let undecided = [
            (ErrorCode::InternalError.code(), "state unavailable"),
            (EthRpcErrorCode::InvalidInput.code(), "execution aborted (timeout = 5s)"),
            (ErrorCode::InvalidParams.code(), "invalid EIP-8130 estimate request: expiry too far"),
        ];

        for (code, message) in undecided {
            let error = ErrorObjectOwned::owned(code, message, None::<()>);
            assert!(PayerRejection::for_phase_zero(&error).is_none(), "{message}");
        }
    }
}
