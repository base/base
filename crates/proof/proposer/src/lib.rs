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
pub use cli::{
    AdminArgs, Cli, HealthArgs, LogArgs, MetricsArgs, ProposerArgs, SignerCli, TxManagerCli,
};

mod config;
pub use config::{ConfigError, ProposerConfig};

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

mod admin;
pub use admin::AdminState;

mod metrics;
pub use metrics::Metrics;

mod service;
pub use service::run;

/// Shared mock implementations for tests.
#[cfg(test)]
pub mod test_utils;
