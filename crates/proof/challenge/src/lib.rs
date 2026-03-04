#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod cli;
pub use cli::{ChallengerArgs, Cli, LogArgs, MetricsArgs};

mod config;
pub use config::{ChallengerConfig, ConfigError, SigningConfig};

mod health;
pub use health::HealthServer;

mod metrics;
pub use metrics::{ChallengerMetrics, INFO, LABEL_VERSION, UP};

mod service;
pub use service::ChallengerService;

mod signal;
pub use signal::SignalHandler;
