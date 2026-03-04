#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod cli;
pub use cli::{ChallengerArgs, Cli, LogArgs, MetricsArgs};

mod config;
pub use config::{ChallengerConfig, ConfigError, SigningConfig};

mod error;
pub use error::{ChallengerError, ChallengerResult};

mod health;
pub use health::serve;

mod metrics;
pub use metrics::{INFO, LABEL_VERSION, UP, record_startup_metrics};

mod service;
pub use service::run;

mod signal;
pub use signal::SignalHandler;
