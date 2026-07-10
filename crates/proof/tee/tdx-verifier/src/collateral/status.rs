//! Intel TCB status parsing and contract-status mapping.

use serde::Deserialize;

use crate::TDXTcbStatus;

/// Intel TCB status values reported by TDX TCB collateral.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum IntelTcbStatus {
    /// Platform TCB is up to date.
    UpToDate = 1,
    /// Platform needs software hardening.
    #[serde(alias = "SWHardeningNeeded")]
    SwHardeningNeeded = 2,
    /// Platform needs configuration hardening.
    ConfigurationNeeded = 3,
    /// Platform needs configuration and software hardening.
    #[serde(alias = "ConfigurationAndSWHardeningNeeded")]
    ConfigurationAndSwHardeningNeeded = 4,
    /// Platform TCB is out of date.
    OutOfDate = 5,
    /// Platform TCB is out of date and needs configuration hardening.
    OutOfDateConfigurationNeeded = 6,
    /// Platform TCB has been revoked.
    Revoked = 7,
    /// Status is not understood by this verifier.
    #[serde(other)]
    Unsupported = 0,
}

impl IntelTcbStatus {
    /// Maps an Intel TCB status into the contract's reduced `TDXTcbStatus`.
    pub fn to_contract_status(self) -> TDXTcbStatus {
        TDXTcbStatus::try_from(self as u8).unwrap_or(TDXTcbStatus::Unknown)
    }

    /// Combines the platform TCB status with the TDX module identity TCB status.
    pub const fn converge_with_tdx_module_status(self, module_status: Self) -> Self {
        match (self, module_status) {
            (_, Self::Revoked) => Self::Revoked,
            (Self::UpToDate | Self::SwHardeningNeeded, Self::OutOfDate) => Self::OutOfDate,
            (
                Self::ConfigurationNeeded | Self::ConfigurationAndSwHardeningNeeded,
                Self::OutOfDate,
            ) => Self::OutOfDateConfigurationNeeded,
            (status, Self::UpToDate | Self::OutOfDate) => status,
            _ => Self::Unsupported,
        }
    }
}
