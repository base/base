#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod balance;
pub use balance::{BALANCE_POLL_INTERVAL, balance_monitor};

mod config;
pub use config::{
    ConfigError, ProposerConfig, RetryConfig, RpcServerConfig, SigningConfig, build_signing_config,
    validate_url,
};

mod constants;
pub use constants::*;

mod contracts;
pub use contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorRoot,
    AnchorStateRegistryClient, AnchorStateRegistryContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameAtIndex, GameInfo, LocalOutputProposer, OutputProposer,
    RemoteOutputProposer, build_proof_data, create_output_proposer, encode_create_calldata,
    encode_extra_data, game_already_exists_selector, is_game_already_exists,
};

mod driver;
pub use driver::{Driver, DriverConfig, DriverHandle, ProposerDriverControl};

mod enclave;
pub use enclave::{
    EnclaveClient, EnclaveClientTrait, PerChainConfig, Proposal, create_enclave_client,
    rollup_config_to_per_chain_config,
};

mod error;
pub use error::*;

mod health;
pub use health::serve;

mod metrics;
pub use metrics::{
    ACCOUNT_BALANCE_WEI, CACHE_HITS_TOTAL, CACHE_MISSES_TOTAL, INFO, L2_OUTPUT_PROPOSALS_TOTAL,
    LABEL_CACHE_NAME, LABEL_VERSION, PROOF_QUEUE_DEPTH, UP, record_startup_metrics,
};

mod prover;
pub use prover::{Prover, ProverProposal};

mod recovery;
pub use recovery::recover_parent_game_state_standalone;

mod rpc;
pub use rpc::{
    CacheMetrics, GenesisL2BlockRef, HttpProvider, L1BlockId, L1BlockRef, L1Client, L1ClientConfig,
    L1ClientImpl, L2BlockRef, L2Client, L2ClientConfig, L2ClientImpl, L2HttpProvider, MeteredCache,
    OpBlock, ProofCacheKey, RethExecutionWitness, RethL2Client, RollupClient, RollupClientConfig,
    RollupClientImpl, RpcError, RpcResult, SyncStatus, create_l2_client,
};

mod service;
pub use service::run;

mod signal;
pub use signal::setup_signal_handler;

#[cfg(test)]
pub mod test_utils;
