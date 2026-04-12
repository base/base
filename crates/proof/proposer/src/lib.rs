#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod cli;
pub use cli::{
    AdminArgs, Cli, HealthArgs, LogArgs, MetricsArgs, ProposerArgs, SignerCli, TxManagerCli,
};

mod config;
pub use config::{ConfigError, ProposerConfig};

mod constants;
pub use constants::{
    MAX_FACTORY_SCAN_LOOKBACK, MAX_PROOF_RETRIES, PROPOSAL_TIMEOUT, PROVER_TIMEOUT,
    RECOVERY_SCAN_CONCURRENCY,
};

mod output_proposer;
pub use output_proposer::{DryRunProposer, OutputProposer, ProposalSubmitter};

mod driver;
pub use driver::{DriverConfig, PipelineHandle, ProposerDriverControl, RecoveredState};

mod pipeline;
pub use pipeline::{PipelineConfig, ProvingPipeline};

mod error;
pub use error::{ProposerError, ProposerResult};

mod admin;
pub use admin::{AdminServer, ProposerAdminApiServer, ProposerAdminApiServerImpl};

mod metrics;
pub use metrics::Metrics;

mod service;
pub use service::ProposerService;

#[cfg(test)]
pub mod test_utils;
