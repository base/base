#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod args;
pub use args::{Args, BuilderSetup, FlashblocksArgs, TransactionEventsArgs};

mod candidates;
pub use candidates::{
    BuilderPath, CandidateArgs, CandidateCapabilities, CandidateWiring,
    DEFAULT_CANDIDATE_MAX_EXPIRY_SECS,
};
