//! Error types for shared contract clients.

use thiserror::Error;

/// Error type for contract interactions.
#[derive(Debug, Error)]
pub enum ContractError {
    /// A contract call or onchain interaction failed.
    #[error("{context}: {source}")]
    Call {
        /// Human-readable label for the failed call (e.g. "`BLOCK_INTERVAL` failed").
        context: String,
        /// The underlying Alloy contract error.
        source: Box<alloy_contract::Error>,
    },

    /// A provider request failed before a contract call was constructed.
    #[error("{context}: {source}")]
    Provider {
        /// Human-readable label for the failed provider request.
        context: String,
        /// The underlying Alloy transport error.
        source: alloy_transport::TransportError,
    },

    /// A value returned by the contract failed a validation check.
    #[error("{0}")]
    Validation(String),
}

impl ContractError {
    /// Creates an error for a failed contract call.
    pub fn call(context: impl Into<String>, source: alloy_contract::Error) -> Self {
        Self::Call { context: context.into(), source: Box::new(source) }
    }

    /// Creates an error for a failed provider request.
    pub fn provider(context: impl Into<String>, source: alloy_transport::TransportError) -> Self {
        Self::Provider { context: context.into(), source }
    }

    /// Creates an error for a failed contract value validation.
    pub fn validation(context: impl Into<String>) -> Self {
        Self::Validation(context.into())
    }

    /// Returns whether a probe failed because the contract does not expose the called method.
    ///
    /// Unknown selectors and empty (`0x`) returns or reverts indicate that
    /// the method is unavailable. Non-empty reverts and ABI decoding
    /// failures are preserved as contract errors.
    #[must_use]
    pub fn is_missing_method(&self) -> bool {
        let Self::Call { source, .. } = self else {
            return false;
        };

        matches!(
            source.as_ref(),
            alloy_contract::Error::UnknownFunction(_)
                | alloy_contract::Error::UnknownSelector(_)
                | alloy_contract::Error::ZeroData(_, _)
        ) || source.as_revert_data().is_some_and(|data| data.is_empty())
    }

    /// Returns whether a contract call reached EVM execution and reverted.
    #[must_use]
    pub fn is_execution_revert(&self) -> bool {
        let Self::Call { source, .. } = self else {
            return false;
        };
        source.as_revert_data().is_some()
    }
}

#[cfg(test)]
mod tests {
    use alloy_contract::Error as AlloyContractError;
    use alloy_sol_types::Error as SolTypesError;
    use alloy_transport::TransportErrorKind;

    use super::ContractError;

    #[test]
    fn missing_method_detects_unknown_function() {
        let err = ContractError::call(
            "probe failed",
            AlloyContractError::UnknownFunction("INTERMEDIATE_BLOCK_INTERVAL".to_string()),
        );

        assert!(err.is_missing_method());
    }

    #[test]
    fn missing_method_detects_unknown_selector() {
        let err = ContractError::call(
            "probe failed",
            AlloyContractError::UnknownSelector([0x12, 0x34, 0x56, 0x78].into()),
        );

        assert!(err.is_missing_method());
    }

    #[test]
    fn missing_method_detects_zero_data_return() {
        let err = ContractError::call(
            "probe failed",
            AlloyContractError::ZeroData(
                "INTERMEDIATE_BLOCK_INTERVAL".to_string(),
                SolTypesError::Overrun.into(),
            ),
        );

        assert!(err.is_missing_method());
    }

    #[test]
    fn missing_method_detects_empty_revert() {
        let source =
            AlloyContractError::TransportError(alloy_transport::TransportError::ErrorResp(
                serde_json::from_str(r#"{"code":3,"message":"execution reverted","data":"0x"}"#)
                    .unwrap(),
            ));

        assert!(ContractError::call("probe failed", source).is_missing_method());
    }

    #[test]
    fn missing_method_rejects_nonempty_revert() {
        let source =
            AlloyContractError::TransportError(alloy_transport::TransportError::ErrorResp(
                serde_json::from_str(
                    r#"{"code":3,"message":"execution reverted","data":"0x1234"}"#,
                )
                .unwrap(),
            ));

        let error = ContractError::call("probe failed", source);
        assert!(!error.is_missing_method());
        assert!(error.is_execution_revert());
    }

    #[test]
    fn missing_method_rejects_abi_decoding_failure() {
        let err =
            ContractError::call("probe failed", AlloyContractError::from(SolTypesError::Overrun));

        assert!(!err.is_missing_method());
    }

    #[test]
    fn missing_method_rejects_transport_error() {
        let err = ContractError::call(
            "probe failed",
            AlloyContractError::TransportError(TransportErrorKind::custom_str("offline")),
        );

        assert!(!err.is_missing_method());
        assert!(!err.is_execution_revert());
    }

    #[test]
    fn missing_method_rejects_validation_error() {
        assert!(!ContractError::validation("invalid value").is_missing_method());
    }
}
