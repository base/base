#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod error;
pub use error::ProofSubmissionError;

mod classifier;
pub use classifier::KnownRevert;

#[cfg(feature = "snark-receipt")]
mod snark_receipt;
#[cfg(feature = "snark-receipt")]
pub use snark_receipt::{SnarkReceiptDecodeError, SnarkReceiptEncoder};

mod submission;
pub use submission::{ChallengeProofSubmission, NullifyProofSubmission};

mod submitter;
pub use submitter::AggregateProofSubmitter;

#[cfg(any(all(test, feature = "snark-receipt"), feature = "test-utils"))]
pub mod test_utils;
