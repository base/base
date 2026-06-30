//! Intel TCB status parsing and contract-status mapping.

use serde::{Deserialize, Deserializer};

use crate::TDXTcbStatus;

/// Intel TCB status values reported by TDX TCB collateral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntelTcbStatus {
    /// Platform TCB is up to date.
    UpToDate,
    /// Platform needs software hardening.
    SwHardeningNeeded,
    /// Platform needs configuration hardening.
    ConfigurationNeeded,
    /// Platform needs configuration and software hardening.
    ConfigurationAndSwHardeningNeeded,
    /// Platform TCB is out of date.
    OutOfDate,
    /// Platform TCB is out of date and needs configuration hardening.
    OutOfDateConfigurationNeeded,
    /// Platform TCB has been revoked.
    Revoked,
    /// Status is not understood by this verifier.
    Unsupported,
}

impl IntelTcbStatus {
    /// Parses an Intel TCB status string.
    pub fn from_intel_str(status: &str) -> Self {
        match status {
            "UpToDate" => Self::UpToDate,
            "SWHardeningNeeded" | "SwHardeningNeeded" => Self::SwHardeningNeeded,
            "ConfigurationNeeded" => Self::ConfigurationNeeded,
            "ConfigurationAndSWHardeningNeeded" | "ConfigurationAndSwHardeningNeeded" => {
                Self::ConfigurationAndSwHardeningNeeded
            }
            "OutOfDate" => Self::OutOfDate,
            "OutOfDateConfigurationNeeded" => Self::OutOfDateConfigurationNeeded,
            "Revoked" => Self::Revoked,
            _ => Self::Unsupported,
        }
    }

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
        match module_status {
            Self::OutOfDate => match self {
                Self::UpToDate | Self::SwHardeningNeeded => Self::OutOfDate,
                Self::ConfigurationNeeded | Self::ConfigurationAndSwHardeningNeeded => {
                    Self::OutOfDateConfigurationNeeded
                }
                status => status,
            },
            Self::Revoked => Self::Revoked,
            Self::UpToDate => self,
            Self::SwHardeningNeeded
            | Self::ConfigurationNeeded
            | Self::ConfigurationAndSwHardeningNeeded
            | Self::OutOfDateConfigurationNeeded
            | Self::Unsupported => Self::Unsupported,
        }
    }

    /// Returns true when a QE identity TCB status is acceptable.
    pub const fn is_accepted_qe_identity_status(self) -> bool {
        matches!(self, Self::UpToDate)
    }
}

impl<'de> Deserialize<'de> for IntelTcbStatus {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_intel_str(&value))
    }
}
