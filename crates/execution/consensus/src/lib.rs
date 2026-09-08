#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

extern crate alloc;

mod proof;
pub use proof::{calculate_receipt_root, calculate_receipt_root_no_memo};

pub mod validation;
pub use validation::{canyon, isthmus, validate_base_time_metadata, validate_block_post_execution};

pub mod error;
pub use error::BaseConsensusError;

mod beacon;
pub use beacon::BaseBeaconConsensus;
mod mode;
pub use mode::ValidationMode;
mod consensus_error;
pub use consensus_error::{
    ConsensusError, HeaderConsensusError, MessageError, ReceiptRootBloom, TransactionRoot,
    TxGasLimitTooHighErr,
};
mod common_validation;
pub use common_validation::*;
#[cfg(any(test, feature = "test-utils"))]
mod test_consensus;
#[cfg(any(test, feature = "test-utils"))]
pub use test_consensus::TestConsensus;
#[cfg(any(test, feature = "test-utils"))]
mod ethereum_test_consensus;
#[cfg(any(test, feature = "test-utils"))]
pub use ethereum_test_consensus::EthereumTestConsensus;
