use std::str::FromStr;

use tracing::warn;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
    Compact,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(Self::Json),
            "compact" => Ok(Self::Compact),
            "pretty" => Ok(Self::Pretty),
            _ => {
                warn!("Invalid log format '{}', defaulting to 'pretty'", s);
                Ok(Self::Pretty)
            }
        }
    }
}

pub fn init_logger(log_level: &str) {
    init_logger_with_format(log_level, LogFormat::Pretty);
}

pub fn init_logger_with_format(log_level: &str, format: LogFormat) {
    let level = match log_level.to_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "info" => tracing::Level::INFO,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => {
            warn!("Invalid log level '{}', defaulting to 'info'", log_level);
            tracing::Level::INFO
        }
    };

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.to_string()));

    match format {
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt::layer().json().flatten_event(true).with_current_span(true))
                .init();
        }
        LogFormat::Compact => {
            tracing_subscriber::registry().with(env_filter).with(fmt::layer().compact()).init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::registry().with(env_filter).with(fmt::layer().pretty()).init();
        }
    }
}
