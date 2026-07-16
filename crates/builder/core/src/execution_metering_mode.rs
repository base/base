use clap::ValueEnum;

/// Mode for execution and resource-throttle metering limits.
///
/// Controls how the builder handles execution metering limits that depend on
/// metering service predictions. These limits can be gradually rolled out via
/// dry-run mode before enforcement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ExecutionMeteringMode {
    /// Metering limits are disabled.
    #[default]
    Off,
    /// Dry-run mode: collect metrics about transactions that would exceed metering limits,
    /// but don't actually reject them.
    DryRun,
    /// Enforce mode: reject transactions that exceed metering limits.
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
