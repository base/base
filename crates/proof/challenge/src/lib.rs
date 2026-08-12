#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod cli;
pub use cli::{ChallengerArgs, Cli, HealthArgs, LogArgs, MetricsArgs, SignerCli, TxManagerCli};

mod anchor;
pub use anchor::AnchorUpdater;

mod config;
pub use config::{ChallengerConfig, ProofProtocolVersion};

mod driver;
pub use driver::{Driver, DriverComponents};

mod pending;
pub use pending::{DisputeIntent, PendingProof, PendingProofs, ProofKind, ProofPhase, ProofUpdate};

mod proof_manager;
pub use proof_manager::DisputeProofManager;

mod proof_adapter;
pub use proof_adapter::ChallengerProofAdapter;

mod error;
pub use error::ChallengeSubmitError;

mod metrics;
pub use metrics::ChallengerMetrics;

mod scanner;
pub use scanner::{CandidateGame, GameCategory, GameScanner};

mod service;
pub use service::ChallengerService;

mod submitter;
pub use submitter::ChallengeSubmitter;

mod validator;
pub use validator::{
    IntermediateValidationParams, OutputValidator, ValidationResult, ValidatorError,
};

mod bond;
pub use bond::{BondManager, BondManagerConfig};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
