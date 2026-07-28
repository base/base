//! Process outcomes and shared optional-value formatting for CLI commands.

/// Whether a command observed failures, used by the binary to set the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command completed without failures; the process should exit 0.
    Success,
    /// The command observed failures; the process should exit non-zero.
    HasFailures,
}

impl CommandOutcome {
    /// Builds an outcome from whether failures were observed.
    pub const fn from_failures(has_failures: bool) -> Self {
        if has_failures { Self::HasFailures } else { Self::Success }
    }

    /// Returns true when the process should exit with a non-zero status.
    pub const fn has_failures(self) -> bool {
        matches!(self, Self::HasFailures)
    }
}

/// Formats optional scalar values for HA command output.
#[derive(Debug)]
pub struct OptionalValue;

impl OptionalValue {
    /// Formats an optional boolean, using `unknown` when unavailable.
    pub const fn boolean(value: Option<bool>) -> &'static str {
        match value {
            Some(true) => "true",
            Some(false) => "false",
            None => "unknown",
        }
    }

    /// Formats an optional `u64`, using `unknown` when unavailable.
    pub fn u64(value: Option<u64>) -> String {
        value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
    }

    /// Formats an optional `u32`, using `unknown` when unavailable.
    pub fn u32(value: Option<u32>) -> String {
        value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
    }
}
