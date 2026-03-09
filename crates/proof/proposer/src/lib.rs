#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod balance;
pub use balance::{BALANCE_POLL_INTERVAL, balance_monitor};

mod cli;
pub use cli::{Cli, LogArgs, MetricsArgs, ProposerArgs, RpcServerArgs};

mod config;
pub use config::{ConfigError, ProposerConfig, RpcServerConfig, SigningConfig};

mod constants;
pub use constants::*;

mod output_proposer;
pub use output_proposer::{
    LocalOutputProposer, OutputProposer, RemoteOutputProposer, build_proof_data,
    create_output_proposer, is_game_already_exists,
};

mod driver;
pub use driver::{Driver, DriverConfig, DriverHandle, ProposerDriverControl, RecoveredGame};

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

mod rpc;
pub use rpc::{L2ClientKind, ProverL2Provider, RethExecutionWitness, RethL2Client};

mod service;
pub use service::run;

mod signal;
pub use signal::SignalHandler;

/// Shared mock implementations for tests.
#[cfg(test)]
pub mod test_utils;
