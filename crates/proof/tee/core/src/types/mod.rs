//! TEE-specific types for account proofs, chain configuration, and output roots.

mod account;
pub use account::{AccountResult, StorageProof};

mod config;
pub use config::{BlockId, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig};

mod output;
pub use output::{output_root_v0, output_root_v0_with_hash};
