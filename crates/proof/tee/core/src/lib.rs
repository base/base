#![doc = include_str!("../README.md")]

mod error;
pub use error::{CryptoError, ProviderError};

mod proof;
pub use proof::{PROOF_TYPE_TEE, ProofEncoder};

mod types;
pub use types::{
    AccountResult, BlockId, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig,
    StorageProof,
};
