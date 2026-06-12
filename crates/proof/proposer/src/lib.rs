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
    MAX_PROOF_RETRIES, PROPOSAL_TIMEOUT, RECOVERY_SCAN_CONCURRENCY, SUBMIT_TIMEOUT,
    SUBMIT_TIMEOUT_SLACK,
};

mod output_proposer;
pub use output_proposer::{DryRunProposer, OutputProposer, ProposalSubmitter};

mod proof_adapter;
pub use proof_adapter::{DispatchedProof, ProofRequesterDispatcher, ProposerProofAdapter};

mod proof_collector;
pub use proof_collector::{
    CollectedProof, ProofCollector, ProofCollectorOrchestrator, ProofCollectorRecoveryProvider,
    ProofCollectorRuntimeConfig, ProofCollectorState, ProofCollectorTickResult, ProofRecoveryCache,
    ProofSubmitEffect, TargetPoll,
};

mod proof_dispatcher;
pub use proof_dispatcher::{
    ProofDispatchAttempt, ProofDispatchOutcome, ProofDispatcher, ProofDispatcherConfig,
    ProofDispatcherRuntimeConfig, ProofDispatcherState, ProofDispatcherTickResult,
};

mod proof_submitter;
pub use proof_submitter::{ProofSubmitter, ProofSubmitterConfig, SubmitAction};

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
