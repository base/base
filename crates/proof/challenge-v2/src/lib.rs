#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod metrics;
pub use metrics::ChallengerMetrics;

mod validate;
pub use validate::{
    AccountProofError, AccountProofVerifier, L2OutputValidator, OutputValidator, ValidationError,
    ValidatorError, Violation, ViolationSituation,
};

mod scanner;
pub use scanner::{ClassifyError, GameDiscovery, GameInfo, GameSituation};

mod submit;
pub use submit::{DisputeAction, DisputeRequest, SubmissionTask};

mod prove;
pub use prove::{ProofError, TeeProofError, TeeProofProvider, TeeProofResult};

mod worker;
pub use worker::{WorkerConfig, WorkerDeps};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
