/// Error type for EIP-1559 parameters.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum EIP1559ParamError {
    /// Thrown if the extra data begins with the wrong version byte.
    #[error("Invalid EIP1559 version byte: {0}")]
    InvalidVersion(u8),
    /// No EIP-1559 parameters provided.
    #[error("No EIP1559 parameters provided")]
    NoEIP1559Params,
    /// Denominator overflow.
    #[error("Denominator overflow")]
    DenominatorOverflow,
    /// Elasticity overflow.
    #[error("Elasticity overflow")]
    ElasticityOverflow,
    /// Extra data is not the correct length.
    #[error("Extra data is not the correct length")]
    InvalidExtraDataLength,
    /// Invalid EIP-1559 parameter combination.
    #[error("EIP-1559 denominator and elasticity must both be zero or both be non-zero")]
    InvalidParams,
    /// Minimum base fee must be None before Jovian.
    #[error("Minimum base fee must be None before Jovian")]
    MinBaseFeeMustBeNone,
    /// Minimum base fee cannot be None after Jovian.
    #[error("Minimum base fee cannot be None after Jovian")]
    MinBaseFeeNotSet,
}
