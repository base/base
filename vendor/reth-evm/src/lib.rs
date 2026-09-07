//! Traits for configuring an EVM specifics.
//!
//! # Revm features
//!
//! This crate does __not__ enforce specific revm features such as `blst` or `c-kzg`, which are
//! critical for revm's evm internals, it is the responsibility of the implementer to ensure the
//! proper features are selected.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod base_build;
pub use base_build::BaseBlockAssembler;

mod base_config;
pub use base_config::{BaseEvmConfig, BaseExecutorFactory, BaseNextBlockEnvAttributes};

mod base_env;
pub use base_env::BaseEvmEnvBuilder;

mod base_receipts;
pub use base_receipts::BaseRethReceiptBuilder;

mod base_payload_env;

use alloc::string::String;
use core::fmt::Debug;

use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::{Address, B256, Bytes};

pub mod either;
/// EVM environment configuration.
pub mod execute;

mod aliases;
pub use aliases::*;

#[cfg(feature = "std")]
mod engine;
#[cfg(feature = "std")]
pub use engine::{ConvertTx, ExecutableTxIterator, ExecutableTxTuple};
mod sender_recovery;
pub use sender_recovery::SenderRecoveryCache;

#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(any(test, feature = "test-utils"))]
/// test helpers for mocking executor
pub mod test_utils;

pub use alloy_evm::{
    block::{OnStateHook, state_changes, system_calls},
    *,
};

/// JIT backend controls exposed by an EVM configuration.
pub trait JitBackend: Send + Sync {
    /// Enables or disables JIT compilation.
    fn set_enabled(&self, enabled: bool) -> Result<(), String>;

    /// Pauses JIT helper execution while keeping queueing and resident compiled code available.
    fn pause(&self);

    /// Resumes background JIT work.
    fn resume(&self);

    /// Clears JIT runtime state.
    fn clear(&self);
}

/// Represents additional attributes required to configure the next block.
///
/// This struct contains all the information needed to build a new block that cannot be
/// derived from the parent block header alone. These attributes are typically provided
/// by the consensus layer (CL) through the Engine API during payload building.
///
/// # Relationship with [`BaseEvmConfig`] and [`execute::BlockAssembler`]
///
/// The flow for building a new block involves:
///
/// 1. **Receive attributes** from the consensus layer containing:
///    - Timestamp for the new block
///    - Fee recipient (coinbase/beneficiary)
///    - Randomness value (prevRandao)
///    - Withdrawals to process
///    - Parent beacon block root for EIP-4788
///
/// 2. **Configure EVM environment** using these attributes: ```rust,ignore let evm_env =
///    evm_config.next_evm_env(&parent, &attributes)?; ```
///
/// 3. **Build the block** with transactions: ```rust,ignore let mut builder =
///    evm_config.builder_for_next_block( &mut state, &parent, attributes )?; ```
///
/// 4. **Assemble the final block** using [`execute::BlockAssembler`] which takes:
///    - Execution results from all transactions
///    - The attributes used during execution
///    - Final state root after all changes
///
/// This design cleanly separates:
/// - **Configuration** (what parameters to use) - handled by `NextBlockEnvAttributes`
/// - **Execution** (running transactions) - handled by `BlockExecutor`
/// - **Assembly** (creating the final block) - handled by `BlockAssembler`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextBlockEnvAttributes {
    /// The timestamp of the next block.
    pub timestamp: u64,
    /// The suggested fee recipient for the next block.
    pub suggested_fee_recipient: Address,
    /// The randomness value for the next block.
    pub prev_randao: B256,
    /// Block gas limit.
    pub gas_limit: u64,
    /// The parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Withdrawals
    pub withdrawals: Option<Withdrawals>,
    /// Optional extra data.
    pub extra_data: Bytes,
    /// Optional slot number for post-Amsterdam payloads.
    pub slot_number: Option<u64>,
}
