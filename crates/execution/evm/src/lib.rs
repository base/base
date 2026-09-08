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

mod base_receipts;
pub use base_receipts::BaseRethReceiptBuilder;

mod base_payload_env;

mod either;
pub use either::Either;
/// EVM environment configuration.
mod execute;
pub use execute::*;

mod aliases;
pub use aliases::*;

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

pub use alloy_evm::{
    block::{OnStateHook, state_changes, system_calls},
    *,
};

mod next_block;
pub use next_block::NextBlockEnvAttributes;

mod cached;
pub use cached::{CachedAccount, CachedReads, CachedReadsDBRef, CachedReadsDbMut};
mod cancelled;
pub use cancelled::{CancelOnDrop, ManualCancel};
mod database;
pub use database::{DatabaseStateProvider, StateProviderDatabase};
#[cfg(feature = "witness")]
mod witness;
#[cfg(feature = "witness")]
pub use witness::ExecutionWitnessRecord;
#[cfg(any(test, feature = "test-utils"))]
mod state_provider_test;
