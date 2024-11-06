//! Error types

/// Error type for EIP-1559 parameters
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EIP1559ParamError {
    /// No EIP-1559 parameters provided
    NoEIP1559Params,
    /// Denominator overflow
    DenominatorOverflow,
    /// Elasticity overflow
    ElasticityOverflow,
}

impl core::fmt::Display for EIP1559ParamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoEIP1559Params => {
                write!(f, "No EIP1559 parameters provided")
            }
            Self::DenominatorOverflow => write!(f, "Denominator overflow"),
            Self::ElasticityOverflow => {
                write!(f, "Elasticity overflow")
            }
        }
    }
}

impl core::error::Error for EIP1559ParamError {}
