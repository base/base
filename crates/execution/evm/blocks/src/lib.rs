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

mod errors;
pub use errors::{BaseBlockExecutionError, L1BlockInfoError};

mod l1;
pub use l1::*;
mod base_build;
pub use base_build::BaseBlockAssembler;

mod base_config;
pub use base_config::{BaseEvmConfig, BaseExecutorFactory, BaseNextBlockEnvAttributes};

mod base_env;
pub use base_env::BaseEvmEnvBuilder;

mod base_payload_env;

/// EVM environment configuration.
mod execute;
pub use execute::*;

#[cfg(feature = "std")]
mod engine;
#[cfg(feature = "std")]
pub use engine::EitherIter;
#[cfg(feature = "std")]
pub use engine::{ConvertTx, ExecutableTxIterator, ExecutableTxTuple};
mod sender_recovery;
pub use sender_recovery::SenderRecoveryCache;

#[cfg(feature = "metrics")]
mod metrics;
#[cfg(feature = "metrics")]
pub use metrics::ExecutorMetrics;
#[cfg(any(test, feature = "test-utils"))]
/// test helpers for mocking executor
pub mod test_utils;

pub use base_execution_evm_runtime as state_changes;
pub use base_execution_evm_runtime as system_calls;
pub use base_execution_evm_runtime::{OnStateHook, *};

mod next_block;
pub use next_block::NextBlockEnvAttributes;

mod cancelled;
pub use cancelled::{CancelOnDrop, ManualCancel};
#[cfg(feature = "witness")]
mod witness;
#[cfg(feature = "witness")]
pub use witness::ExecutionWitnessRecord;
#[cfg(any(test, feature = "test-utils"))]
mod state_provider_test;

mod proof;
pub use proof::{calculate_receipt_root, calculate_receipt_root_no_memo};

mod validation;
pub use validation::*;

mod error;
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

mod gas_limits;
pub use gas_limits::{MAXIMUM_GAS_LIMIT_BLOCK, MINIMUM_GAS_LIMIT};

mod block_import_error;
pub use block_import_error::{
    InsertBlockErrorKind, InsertBlockFatalError, InsertBlockValidationError,
};
