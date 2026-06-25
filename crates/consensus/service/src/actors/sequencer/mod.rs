//! The `SequencerActor` and its components.

mod build;
pub use build::{BuildOutcome, PayloadBuilder, UnsealedPayloadHandle};

mod config;
pub use config::{SequencerConfig, SequencerSyncMode};

mod l1_origin;
#[cfg(test)]
pub use l1_origin::MockOriginSelector;
pub use l1_origin::{
    DelayedL1OriginSelectorProvider, L1OriginSelector, L1OriginSelectorError,
    L1OriginSelectorProvider, OriginSelector, PrefetchedChainProvider,
    PrefetchedChainProviderError, PreparedL1Origin,
};

mod recovery;
pub use recovery::RecoveryModeGuard;

mod seal;
pub use seal::{PayloadSealer, SealState, SealStepError, SealStepOutcome};

mod shadow_cycle;
pub use shadow_cycle::{ShadowCycle, ShadowReconciliationTask};

mod shadow_funding;
pub use shadow_funding::ShadowFunding;

mod engine_request_coordinator;
pub use engine_request_coordinator::SequencerEngineRequestCoordinator;

mod shadow_reconciliation;
pub use shadow_reconciliation::{
    CanonicalReconciliationInputs, CanonicalUnsafeCatchup, SequencerEngineState,
    ShadowReconciliationGate,
};

mod ticker;
pub use ticker::ScheduledTicker;

mod pool;
pub use pool::PoolActivation;

mod pipeline_state;
pub use pipeline_state::BuildPipelineState;

mod shadow_state;
pub use shadow_state::ShadowSequencingState;

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
