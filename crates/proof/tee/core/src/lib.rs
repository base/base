#![doc = include_str!("../README.md")]

// Re-export commonly used types from alloy
pub use alloy_consensus::Header;
// Re-export base_consensus_genesis types for ecosystem compatibility
pub use alloy_eips::eip1898::BlockNumHash;
pub use alloy_primitives::{Address, B256, Bytes, U256};
pub use base_alloy_consensus::OpReceiptEnvelope;
pub use base_consensus_genesis::{
    ChainConfig, ChainGenesis, HardForkConfig, L1ChainConfig, SystemConfig,
};

mod config;
pub use config::{
    default_l1_config, default_rollup_config, l1_config_for_l2_chain_id, sepolia_l1_config,
};

mod error;
pub use error::{ConfigError, CryptoError, EnclaveError, ExecutorError, ProviderError, Result};

mod executor;
pub use executor::{
    BlockExecutionResult, DEPOSIT_EVENT_TOPIC, EnclaveEvmFactory, EnclaveTrieDB, EnclaveTrieHinter,
    ExecutionResult, ExecutionWitness, L1BlockInfo, MAX_SEQUENCER_DRIFT_FJORD, Oracle,
    TransformedWitness, TrieProviderError, build_l1_block_info_from_deposit, execute_block,
    execute_stateless, extract_deposits_from_receipts, l2_block_to_block_info, transform_witness,
    validate_not_deposit, validate_sequencer_drift, verify_execution_result,
};

mod providers;
pub use providers::{
    BlockInfoWrapper, L1ReceiptsFetcher, L2SystemConfigFetcher, compute_l1_receipt_root,
    compute_receipt_root, compute_tx_root,
};

mod serde_utils;

mod types;
pub use types::{
    AccountResult, AggregateRequest, BlockId, ExecuteStatelessRequest, Genesis,
    GenesisSystemConfig, MARSHAL_BINARY_SIZE, PerChainConfig, Proposal, ProposalParams,
    RollupConfig, SIGNATURE_LENGTH, StorageProof, output_root_v0, output_root_v0_with_hash,
};
