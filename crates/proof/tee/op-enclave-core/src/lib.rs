//! Core types for op-enclave.
//!
//! This crate provides Rust equivalents of the Go types used in op-enclave,
//! with serialization that matches the Go `encoding/json` output exactly.

pub mod config;
pub mod error;
pub mod executor;
pub mod providers;
pub mod serde_utils;
pub mod types;

pub use types::account::AccountResult;
pub use types::config::{
    BlockId, Genesis, GenesisSystemConfig, MARSHAL_BINARY_SIZE, PerChainConfig, RollupConfig,
};
pub use types::output::output_root_v0;
pub use types::proposal::{Proposal, ProposalParams};
pub use types::rpc::{AggregateRequest, ExecuteStatelessRequest};

// Re-export error types
pub use error::{ConfigError, CryptoError, EnclaveError, ExecutorError, ProviderError, Result};

// Re-export executor types
pub use executor::{
    DEPOSIT_EVENT_TOPIC, EnclaveTrieDB, ExecutionResult, ExecutionWitness, L1_ATTRIBUTES_DEPOSITOR,
    L1_ATTRIBUTES_PREDEPLOYED, L2_TO_L1_MESSAGE_PASSER, MAX_SEQUENCER_DRIFT_FJORD,
    TransformedWitness, execute_stateless, extract_deposits_from_receipts, l2_block_to_block_info,
    transform_witness, validate_not_deposit, validate_sequencer_drift,
};

// Re-export commonly used types from alloy
pub use alloy_consensus::Header;
pub use alloy_primitives::{Address, B256, Bytes, U256};
pub use base_alloy_consensus::OpReceiptEnvelope;

// Re-export kona_genesis types for ecosystem compatibility
pub use alloy_eips::eip1898::BlockNumHash;
pub use kona_genesis::{ChainConfig, ChainGenesis, HardForkConfig, L1ChainConfig, SystemConfig};

// Re-export provider types
pub use providers::{
    BlockInfoWrapper, L1ReceiptsFetcher, L2SystemConfigFetcher, compute_receipt_root,
    compute_tx_root,
};
