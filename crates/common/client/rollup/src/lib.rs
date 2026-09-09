#![doc = include_str!("../README.md")]

mod provider_ext;
pub use provider_ext::{DebugProviderExt, OptimismRollupProviderExt};

mod types;
pub use types::{GenesisL2BlockRef, L1BlockId, L1BlockRef, L2BlockRef, OutputAtBlock, SyncStatus};
