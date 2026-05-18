#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod game_discovery;
pub use game_discovery::{ClassifyError, GameDiscovery, GameInfo, ProvingState};

mod account_proof;
pub use account_proof::{AccountProofError, AccountProofVerifier};

mod output_validator;
pub use output_validator::{L2OutputValidator, OutputRootError, OutputValidator};

mod violation;
pub use violation::{ValidationError, Violation, ViolationKind};

mod tee_provider;
pub use tee_provider::{TeeProofError, TeeProofProvider, TeeProofResult};

mod prove;
pub use prove::ProofError;

mod dispute_action;
pub use dispute_action::{DisputeAction, DisputeRequest};

mod bond_action;
pub use bond_action::{BondAction, BondRequest};

mod submission;
pub use submission::{Submission, SubmissionHandle, SubmissionTask, SubmitError};

mod game_worker;
pub use game_worker::{GameWorkerConfig, GameWorkerDeps, run_game_worker};

mod game_pool;
pub use game_pool::GamePool;

mod bond_discovery;
pub use bond_discovery::{BondCandidate, BondDiscovery};

mod bond_worker;
pub use bond_worker::{BondError, BondWorkerDeps, run_bond_worker};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
