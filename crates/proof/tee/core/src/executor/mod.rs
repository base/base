//! Stateless block execution module.
//!
//! This module provides the core stateless block execution functionality
//! for running L2 blocks within an enclave without maintaining full state.
//!
//! # Overview
//!
//! The executor validates and (in future implementations) executes L2 blocks
//! using a witness that provides the necessary state data. This enables
//! trustless block verification within a secure enclave.
//!
//! # Components
//!
//! - [`stateless`]: Core stateless execution logic
//! - [`witness`]: Execution witness types and transformation
//! - [`trie_db`]: `TrieDB` provider for state access during execution
//! - [`attributes`]: Payload attributes builder for deposit extraction
//! - [`evm`]: EVM execution wrapper for block execution
//! - [`l2_block_ref`]: L2 block reference parsing for L1 origin validation
//!
//! # Usage
//!
//! ```ignore
//! use base_enclave::executor::{execute_stateless, ExecutionWitness};
//!
//! let result = execute_stateless(
//!     &rollup_config,
//!     &l1_origin,
//!     &l1_receipts,
//!     &previous_block_txs,
//!     &block_header,
//!     &sequenced_txs,
//!     witness,
//!     &message_account,
//! )?;
//! ```

mod attributes;
mod evm;
mod l2_block_ref;
mod stateless;
mod trie_db;
mod witness;

pub use attributes::{
    DEPOSIT_EVENT_TOPIC, L1_ATTRIBUTES_DEPOSITOR, L1_ATTRIBUTES_PREDEPLOYED,
    extract_deposits_from_receipts,
};
pub use evm::{
    BlockExecutionResult, EnclaveEvmFactory, EnclaveTrieHinter, L1BlockInfo,
    build_l1_block_info_from_deposit, execute_block, verify_execution_result,
};
pub use l2_block_ref::l2_block_to_block_info;
pub use stateless::{
    ExecutionResult, L2_TO_L1_MESSAGE_PASSER, MAX_SEQUENCER_DRIFT_FJORD, execute_stateless,
    validate_not_deposit, validate_sequencer_drift,
};
pub use trie_db::{EnclaveTrieDB, TrieProviderError};
pub use witness::{ExecutionWitness, TransformedWitness, transform_witness};
