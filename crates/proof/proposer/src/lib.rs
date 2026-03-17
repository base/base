#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod balance;
pub use balance::{BALANCE_POLL_INTERVAL, balance_monitor};

mod cli;
pub use cli::{Cli, LogArgs, MetricsArgs, ProposerArgs, RpcServerArgs, SignerCli, TxManagerCli};

mod config;
pub use config::{ConfigError, ProposerConfig, RpcServerConfig};

mod constants;
pub use constants::*;

mod output_proposer;
pub use output_proposer::{
    OutputProposer, ProposalSubmitter, build_proof_data, is_game_already_exists,
};

mod driver;
pub use driver::{Driver, DriverConfig, DriverHandle, ProposerDriverControl, RecoveredGame};

mod enclave;
pub use enclave::{EnclaveClientTrait, create_enclave_client, rollup_config_to_per_chain_config};

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

/// Shared mock implementations for tests.
#[cfg(test)]
pub mod test_utils;
