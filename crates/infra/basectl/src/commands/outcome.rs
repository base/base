//! Process outcomes for CLI commands.

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
