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
    DryRunProposer, OutputProposer, ProposalSubmitter, build_proof_data, is_game_already_exists,
};

mod driver;
pub use driver::{
    DriverConfig, PipelineConfig, PipelineHandle, ProposerDriverControl, ProvingPipeline,
    RecoveredState,
};

mod error;
pub use error::*;

mod health;
pub use health::serve;

mod metrics;
pub use metrics::{
    ACCOUNT_BALANCE_WEI, INFO, L2_OUTPUT_PROPOSALS_TOTAL, LABEL_VERSION, UP, record_startup_metrics,
};

mod service;
pub use service::run;

/// Shared mock implementations for tests.
#[cfg(test)]
pub mod test_utils;
