//! Core types for op-enclave.
//!
//! This crate provides Rust equivalents of the Go types used in op-enclave,
//! with serialization that matches the Go `encoding/json` output exactly.

pub mod config;
pub mod error;
pub mod providers;
pub mod serde_utils;
pub mod types;

pub use types::account::AccountResult;
pub use types::config::{
    BlockId, Genesis, GenesisSystemConfig, MARSHAL_BINARY_SIZE, PerChainConfig, RollupConfig,
};
pub use types::proposal::Proposal;

// Re-export error types
pub use error::{ConfigError, CryptoError, EnclaveError, ProviderError, Result};

// Re-export commonly used types from alloy
pub use alloy_consensus::Header;
pub use alloy_primitives::{Address, B256, Bytes, U256};
pub use op_alloy_consensus::OpReceiptEnvelope;

// Re-export kona_genesis types for ecosystem compatibility
pub use alloy_eips::eip1898::BlockNumHash;
pub use kona_genesis::{ChainGenesis, HardForkConfig, SystemConfig};

// Re-export provider types
pub use providers::{
    BlockInfoWrapper, L1ReceiptsFetcher, L2SystemConfigFetcher, compute_receipt_root,
    compute_tx_root,
};
