#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    UpgradeSignalArgs, UpgradeSignalBlockTag, UpgradeSignalConfig, UpgradeSignalConfigError,
    UpgradeSignalDefaults, UpgradeSignalL1RpcArgs, UpgradeSignalMode, UpgradeSignalStartupConfig,
    UpgradeSignalStartupMode,
};

mod contract;
pub use contract::AlloyUpgradeSignalReader;

mod error;
pub use error::UpgradeSignalError;

mod metrics;
pub use metrics::{UpgradeSignalMetricLayer, UpgradeSignalMetrics};

mod runtime;
pub use runtime::{
    RuntimeRegistrySink, UpgradeSignalApplyAction, UpgradeSignalApplyChange,
    UpgradeSignalApplySummary, UpgradeSignalRefresher, UpgradeSignalRuntimeApplier,
    UpgradeSignalRuntimeValidation,
};

mod state;
pub use state::{UpgradeSignal, UpgradeSignalMonitor, UpgradeSignalSchedule};
