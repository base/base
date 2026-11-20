#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::UpgradeSignalMode;
#[cfg(feature = "reader")]
pub use config::{
    StartupScheduleAction, UpgradeSignalArgs, UpgradeSignalBlockTag, UpgradeSignalConfig,
    UpgradeSignalDefaults, UpgradeSignalL1RpcArgs, UpgradeSignalStartupConfig,
    UpgradeSignalStartupMode,
};

#[cfg(feature = "reader")]
mod contract;
#[cfg(feature = "reader")]
pub use contract::AlloyUpgradeSignalReader;

#[cfg(feature = "reader")]
mod error;
#[cfg(feature = "reader")]
pub use error::UpgradeSignalError;

#[cfg(feature = "reader")]
mod ids;
#[cfg(feature = "reader")]
pub use ids::ContractUpgradeIds;

#[cfg(feature = "reader")]
mod metrics;
#[cfg(feature = "reader")]
pub use metrics::{UpgradeSignalMetricLayer, UpgradeSignalMetrics};

mod runtime;
pub use runtime::{
    UpgradeSignalApplyAction, UpgradeSignalApplyChange, UpgradeSignalApplySummary,
};
#[cfg(feature = "reader")]
pub use runtime::{RuntimeRegistrySink, UpgradeSignalRefresher, UpgradeSignalRuntimeApplier};

#[cfg(feature = "reader")]
mod state;
#[cfg(feature = "reader")]
pub use state::{
    UpgradeSignal, UpgradeSignalMonitor, UpgradeSignalPollOutcome, UpgradeSignalSchedule,
};

mod readiness;
pub use readiness::{UpgradeReadiness, UpgradeReadinessEntry};

#[cfg(feature = "reader")]
mod version;
#[cfg(feature = "reader")]
pub use version::PackedProtocolVersion;

#[cfg(all(test, feature = "reader"))]
pub mod test_utils;
