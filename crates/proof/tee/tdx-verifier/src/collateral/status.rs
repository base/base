//! Intel TCB status parsing and contract-status mapping.

use serde::Deserialize;

use crate::TDXTcbStatus;

/// Intel TCB status values reported by TDX TCB collateral.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum IntelTcbStatus {
    /// Platform TCB is up to date.
    UpToDate,
    /// Platform needs software hardening.
    #[serde(alias = "SWHardeningNeeded")]
    SwHardeningNeeded,
    /// Platform needs configuration hardening.
    ConfigurationNeeded,
    /// Platform needs configuration and software hardening.
    #[serde(alias = "ConfigurationAndSWHardeningNeeded")]
    ConfigurationAndSwHardeningNeeded,
    /// Platform TCB is out of date.
    OutOfDate,
    /// Platform TCB is out of date and needs configuration hardening.
    OutOfDateConfigurationNeeded,
    /// Platform TCB has been revoked.
    Revoked,
    /// Status is not understood by this verifier.
    #[serde(other)]
    Unsupported,
}

impl IntelTcbStatus {
    /// Maps an Intel TCB status into the contract's reduced `TDXTcbStatus`.
    pub const fn to_contract_status(self) -> TDXTcbStatus {
        match self {
            Self::UpToDate => TDXTcbStatus::UpToDate,
            Self::SwHardeningNeeded => TDXTcbStatus::SwHardeningNeeded,
            Self::ConfigurationNeeded => TDXTcbStatus::ConfigurationNeeded,
            Self::ConfigurationAndSwHardeningNeeded => {
                TDXTcbStatus::ConfigurationAndSwHardeningNeeded
            }
            Self::OutOfDate => TDXTcbStatus::OutOfDate,
            Self::OutOfDateConfigurationNeeded => TDXTcbStatus::OutOfDateConfigurationNeeded,
            Self::Revoked => TDXTcbStatus::Revoked,
            Self::Unsupported => TDXTcbStatus::Unknown,
        }
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
