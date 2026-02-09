use clap::ValueEnum;

/// Resource metering mode for transaction time limits.
///
/// Controls how the builder handles time-based resource limits
/// (execution time and state root calculation time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ResourceMeteringMode {
    /// Resource metering is disabled. No time limit checks are performed.
    #[default]
    Off,
    /// Dry-run mode: collect metrics about transactions that would exceed time limits,
    /// but don't actually reject them. Useful for gathering data before enforcement.
    DryRun,
    /// Enforce mode: reject transactions that exceed time limits.
    Enforce,
}

impl ResourceMeteringMode {
    /// Returns true if metering data should be collected (dry-run or enforce mode).
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::DryRun | Self::Enforce)
    }

    /// Returns true if limits should only be observed in dry-run mode, not enforced.
    pub const fn is_dry_run(&self) -> bool {
        matches!(self, Self::DryRun)
    }
}
