#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod entrypoints;

pub mod events;
pub use events::MempoolEvent;

pub mod mempool;
pub use mempool::{Mempool, PoolConfig};

pub mod reputation;
pub use reputation::{ReputationService, ReputationStatus};

pub mod types;
pub use types::{
    UserOpHash, UserOperationRequest, ValidationResult, VersionedUserOperation,
    WrappedUserOperation,
};
