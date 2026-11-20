//! Runtime upgrade signal application.

#[cfg(feature = "reader")]
mod refresher;
#[cfg(feature = "reader")]
pub use refresher::UpgradeSignalRefresher;

#[cfg(feature = "reader")]
mod sink;
#[cfg(feature = "reader")]
pub use sink::{RuntimeRegistrySink, UpgradeSignalRuntimeApplier};

mod summary;
pub use summary::{UpgradeSignalApplyAction, UpgradeSignalApplyChange, UpgradeSignalApplySummary};
