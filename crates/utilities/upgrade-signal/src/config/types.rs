use core::time::Duration;

use alloy_rpc_types_eth::BlockNumberOrTag;

/// Controls which local schedule mutation paths are enabled for the L1 upgrade signal.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeSignalMode {
    /// Do not mutate local upgrade schedules; live L1 metrics are still observed.
    #[default]
    MetricsOnly,
    /// Apply the L1 signal once before startup; live polling remains metrics-only.
    StartupApply,
    /// Apply the L1 signal before startup, automatically re-apply observed live L1 changes,
    /// and expose manual runtime admin refresh.
    RuntimeAdmin,
}

impl UpgradeSignalMode {
    /// Returns true if this mode applies the schedule before node startup.
    pub const fn applies_at_startup(self) -> bool {
        matches!(self, Self::StartupApply | Self::RuntimeAdmin)
    }

    /// Returns true if this mode applies live L1 schedule changes after startup (and fails closed
    /// on an unsupportable one).
    ///
    /// Only [`Self::RuntimeAdmin`] tracks the schedule live; the other modes observe it for metrics
    /// but never apply or halt on a change seen after startup. Upgrade-readiness reporting uses this
    /// to avoid claiming a node will follow the current L1 schedule when its mode never will.
    pub const fn applies_live_schedule(self) -> bool {
        matches!(self, Self::RuntimeAdmin)
    }

    /// Returns true if this mode allows manual runtime schedule refresh.
    pub const fn allows_runtime_admin(self) -> bool {
        matches!(self, Self::RuntimeAdmin)
    }
}

/// L1 block tag used when reading the upgrade signal contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum UpgradeSignalBlockTag {
    /// Read at the latest finalized L1 block. Reorg-safe; recommended for production.
    #[default]
    Finalized,
    /// Read at the latest safe L1 block.
    Safe,
    /// Read at the latest L1 block. May reorg; useful for devnets without L1 finality.
    Latest,
}

impl UpgradeSignalBlockTag {
    /// Converts to the alloy block tag used by the contract reader.
    pub const fn block_number_or_tag(self) -> BlockNumberOrTag {
        match self {
            Self::Finalized => BlockNumberOrTag::Finalized,
            Self::Safe => BlockNumberOrTag::Safe,
            Self::Latest => BlockNumberOrTag::Latest,
        }
    }

    /// Interval between live contract reads at this tag, matching how often the tag typically
    /// advances: one L1 slot for `latest`, one epoch for `safe`, and the two-epoch finality lag
    /// rounded up for `finalized`.
    pub const fn poll_interval(self) -> Duration {
        match self {
            Self::Finalized => Duration::from_secs(15 * 60),
            Self::Safe => Duration::from_secs(32 * 12),
            Self::Latest => Duration::from_secs(12),
        }
    }
}

/// Controls whether a service should perform its own startup signal read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpgradeSignalStartupMode {
    /// Read and apply the configured signal according to [`UpgradeSignalMode`].
    #[default]
    ReadAndApply,
    /// The caller has already applied the startup signal.
    AlreadyApplied,
}

impl UpgradeSignalStartupMode {
    /// Returns true if the service should perform its own startup signal read.
    pub const fn reads_and_applies(self) -> bool {
        matches!(self, Self::ReadAndApply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_grows_with_tag_lag() {
        assert!(
            UpgradeSignalBlockTag::Latest.poll_interval()
                < UpgradeSignalBlockTag::Safe.poll_interval()
        );
        assert!(
            UpgradeSignalBlockTag::Safe.poll_interval()
                < UpgradeSignalBlockTag::Finalized.poll_interval()
        );
    }
}
