#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod cli;
pub use cli::{ChallengerArgs, Cli, LogArgs, MetricsArgs, SignerCli};

mod config;
pub use config::{ChallengerConfig, ConfigError, UrlValidationError, Validated};

mod error;
pub use error::ChallengeSubmitError;

mod health;
pub use health::HealthServer;

mod metrics;
pub use metrics::ChallengerMetrics;

mod scanner;
pub use scanner::{CandidateGame, GameScanner, ScannerConfig};

mod service;
pub use service::ChallengerService;

mod submitter;
pub use submitter::ChallengeSubmitter;

mod validator;
pub use validator::{
    IntermediateValidationParams, OutputValidator, ValidationResult, ValidatorError,
};

#[cfg(test)]
pub mod test_utils;
