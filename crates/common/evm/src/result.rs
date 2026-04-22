//! Contains the `[BaseHaltReason]` type.
use revm::context_interface::result::HaltReason;

/// Base halt reason.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BaseHaltReason {
    /// Base halt reason.
    Base(HaltReason),
    /// Failed deposit halt reason.
    FailedDeposit,
}

impl From<HaltReason> for BaseHaltReason {
    fn from(value: HaltReason) -> Self {
        Self::Base(value)
    }
}

impl TryFrom<BaseHaltReason> for HaltReason {
    type Error = BaseHaltReason;

    fn try_from(value: BaseHaltReason) -> Result<Self, BaseHaltReason> {
        match value {
            BaseHaltReason::Base(reason) => Ok(reason),
            BaseHaltReason::FailedDeposit => Err(value),
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use revm::context_interface::result::OutOfGasError;

    use super::*;

    #[test]
    fn test_serialize_json_base_halt_reason() {
        let response = r#"{"Base":{"OutOfGas":"Basic"}}"#;

        let base_halt_reason: BaseHaltReason = serde_json::from_str(response).unwrap();
        assert_eq!(base_halt_reason, HaltReason::OutOfGas(OutOfGasError::Basic).into());
    }
}
