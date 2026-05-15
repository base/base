#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod metrics;
pub use metrics::ChallengerMetrics;

mod game_discovery;
pub use game_discovery::{ClassifyError, GameDiscovery, GameInfo, GameSituation};

mod account_proof;
pub use account_proof::{AccountProofError, AccountProofVerifier};

mod output_validator;
pub use output_validator::{L2OutputValidator, OutputValidator, ValidatorError};

mod violation;
pub use violation::{ValidationError, Violation, ViolationSituation};

mod tee_provider;
pub use tee_provider::{TeeProofError, TeeProofProvider, TeeProofResult};

mod prove;
pub use prove::ProofError;

mod dispute_action;
pub use dispute_action::{DisputeAction, DisputeRequest};

mod submission;
pub use submission::SubmissionTask;

mod game_worker;
pub use game_worker::{WorkerConfig, WorkerDeps, run_game_worker};

mod game_pool;
pub use game_pool::GamePool;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
