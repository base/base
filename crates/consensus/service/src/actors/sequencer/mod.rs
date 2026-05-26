//! The `SequencerActor` and its components.

mod build;
pub use build::{PayloadBuilder, UnsealedPayloadHandle};

mod config;
pub use config::{SequencerCadenceConfig, SequencerConfig};

mod origin_selector;
#[cfg(test)]
pub use origin_selector::MockOriginSelector;
pub use origin_selector::{
    DelayedL1OriginSelectorProvider, L1OriginSelector, L1OriginSelectorError,
    L1OriginSelectorProvider, OriginSelector,
};

mod recovery;
pub use recovery::RecoveryModeGuard;

mod seal;
pub use seal::{PayloadSealer, SealState, SealStepError, SealStepOutcome};

mod ticker;
pub use ticker::ScheduledTicker;

mod timestamp;
pub use timestamp::{
    SequencerTimestamp, SequencerTimestampPlanner, SequencerTimestampPlannerError,
};

mod pool;
pub use pool::PoolActivation;

mod actor;
pub use actor::{PendingStopSender, SequencerActor};

mod admin_api_impl;
pub use admin_api_impl::SequencerAdminQuery;

mod metrics;

mod error;
pub use error::SequencerActorError;

mod conductor;
#[cfg(test)]
pub use conductor::MockConductor;
pub use conductor::{Conductor, ConductorClient, ConductorError};

mod engine_client;

#[cfg(test)]
pub use engine_client::MockSequencerEngineClient;
pub use engine_client::{QueuedSequencerEngineClient, SequencerEngineClient};

#[cfg(test)]
mod tests;
