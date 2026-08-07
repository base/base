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
    pub fn is_missing_method(&self) -> bool {
        let Self::Call { source, .. } = self else {
            return false;
        };

        matches!(
            source.as_ref(),
            alloy_contract::Error::UnknownFunction(_)
                | alloy_contract::Error::UnknownSelector(_)
                | alloy_contract::Error::ZeroData(_, _)
                | alloy_contract::Error::AbiError(_)
        ) || source.as_revert_data().is_some()
    }
}

#[cfg(test)]
mod tests {
    use alloy_contract::Error as AlloyContractError;
    use alloy_sol_types::Error as SolTypesError;
    use alloy_transport::TransportErrorKind;

    use super::ContractError;

    #[test]
    fn missing_method_classification_preserves_transport_and_validation_errors() {
        let missing =
            ContractError::call("probe failed", AlloyContractError::from(SolTypesError::Overrun));
        let transport = ContractError::call(
            "probe failed",
            AlloyContractError::TransportError(TransportErrorKind::custom_str("offline")),
        );

        assert!(missing.is_missing_method());
        assert!(!transport.is_missing_method());
        assert!(!ContractError::validation("invalid value").is_missing_method());
    }
}
