use core::{fmt, str::FromStr};

/// Mode for execution metering limits (execution time and state root time).
///
/// Controls how the builder handles execution metering limits that depend on
/// metering service predictions. These limits can be gradually rolled out
/// via dry-run mode before enforcement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMeteringMode {
    /// Execution metering limits are disabled. No time limit checks are performed.
    #[default]
    Off,
    /// Dry-run mode: collect metrics about transactions that would exceed time limits,
    /// but don't actually reject them. Useful for gathering data before enforcement.
    DryRun,
    /// Enforce mode: reject transactions that exceed time limits.
    Enforce,
}

impl ExecutionMeteringMode {
    /// Returns true if metering data should be collected (dry-run or enforce mode).
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::DryRun | Self::Enforce)
    }

    /// Returns true if limits should only be observed in dry-run mode, not enforced.
    pub const fn is_dry_run(&self) -> bool {
        matches!(self, Self::DryRun)
    }
}

impl fmt::Display for ExecutionMeteringMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::DryRun => write!(f, "dry-run"),
            Self::Enforce => write!(f, "enforce"),
        }
    }
}

impl FromStr for ExecutionMeteringMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "dry-run" => Ok(Self::DryRun),
            "enforce" => Ok(Self::Enforce),
            other => Err(format!(
                "invalid execution metering mode '{other}', expected one of: off, dry-run, enforce"
            )),
        }
    }
}
