//! Batcher driver state machine, submissions, throttling, and control.

mod error;
pub use error::BatchDriverError;

mod outcome;
pub use outcome::TxOutcome;

mod throttle;
pub use throttle::{
    DaThrottle, ThrottleConfig, ThrottleController, ThrottleInfo, ThrottleParams, ThrottleStrategy,
};

mod throttle_client;
pub use throttle_client::{NoopThrottleClient, ThrottleClient};

mod submissions;
pub use submissions::{BatchTxCandidateBuilder, BatchTxCandidateError, SubmissionQueue};

mod config;
pub use config::BatchDriverConfig;

mod event;
pub use event::DriverEvent;

mod derivation_status;
pub use derivation_status::DerivationStatus;

mod admin;
pub use admin::{
    ADMIN_CHANNEL_CAPACITY, AdminCommand, AdminError, AdminHandle, AdminResult, BatcherStatus,
};

mod driver;
pub use driver::{BatchDriver, BatchDriverHeads};

pub mod test_utils;
