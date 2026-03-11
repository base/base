#![doc = include_str!("../README.md")]

pub use alloy_consensus::Header;
pub use alloy_primitives::{Address, B256, Bytes, U256};
pub use base_consensus_genesis::ChainConfig;

mod error;
pub use error::{CryptoError, ProviderError};

mod proof;
pub use proof::{PROOF_TYPE_TEE, ProofEncoder};

mod types;
pub use types::{
    AccountResult, AggregateRequest, BlockId, ExecuteStatelessRequest, ExecutionWitness, Genesis,
    GenesisSystemConfig, L2BlockRefError, PerChainConfig, RollupConfig, StorageProof,
    l2_block_to_block_info, output_root_v0, output_root_v0_with_hash,
};
