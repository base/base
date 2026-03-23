//! TEE-specific types for account proofs, chain configuration, and output roots.

mod account;
pub use account::{AccountResult, StorageProof};

mod config;
pub use config::{BlockId, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig};
