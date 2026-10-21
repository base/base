//! ERC-8168 `PAYER_REJECTED` errors for transactions rejected before they are forwarded.

use alloy_rpc_types_eth::error::EthRpcErrorCode;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};
use serde_json::{Value, json};

/// Builds the `PAYER_REJECTED` errors a payer returns, for `payer_sendTransaction`
/// transactions the node rejects itself.
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

    /// The rejection for a phase-0 simulation that failed with `error`: `EXECUTION_REVERTED`
    /// for a revert, carrying its data, and `INVALID_TRANSACTION` for a halt or a transaction
    /// the simulation refuses.
    ///
    /// Returns `None` when the simulation could not run, which says nothing about the
    /// transaction.
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
        let refused = [
            ErrorCode::InvalidParams.code(),
            EthRpcErrorCode::InvalidInput.code(),
            EthRpcErrorCode::TransactionRejected.code(),
        ];
        refused.contains(&code).then(|| Self::invalid_transaction(error.message()))
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
        let halt = ErrorObjectOwned::owned(
            EthRpcErrorCode::TransactionRejected.code(),
            "out of gas",
            None::<()>,
        );

        let rejection = PayerRejection::for_phase_zero(&halt).unwrap();

        assert_eq!(
            data(&rejection),
            json!({ "code": "INVALID_TRANSACTION", "reason": "out of gas" })
        );
    }

    #[test]
    fn passes_a_simulation_that_could_not_run() {
        let internal = ErrorObjectOwned::owned(
            ErrorCode::InternalError.code(),
            "state unavailable",
            None::<()>,
        );

        assert!(PayerRejection::for_phase_zero(&internal).is_none());
    }
}
