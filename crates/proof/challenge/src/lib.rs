#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod cli;
pub use cli::{ChallengerArgs, Cli, LogArgs, MetricsArgs};

mod config;
pub use config::{ChallengerConfig, ConfigError, SigningConfig, UrlValidationError, Validated};

mod health;
pub use health::HealthServer;

mod metrics;
pub use metrics::ChallengerMetrics;

mod scanner;
pub use scanner::{CandidateGame, GameScanner, ScannerConfig};

mod service;
pub use service::ChallengerService;

#[cfg(test)]
mod test_utils;
