//! Logging Configuration Types

use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tracing::level_filters::LevelFilter;

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Full format with timestamp, level, target, and spans.
    #[default]
    Full,
    /// JSON format for structured logging.
    Json,
    /// Pretty format with colors (for development).
    Pretty,
    /// Compact format with minimal metadata.
    Compact,
    /// Logfmt format (key=value pairs).
    Logfmt,
}

/// Log rotation strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogRotation {
    /// Rotate every minute (for testing).
    Minutely,
    /// Rotate every hour.
    Hourly,
    /// Rotate every day.
    Daily,
    /// Never rotate (default).
    #[default]
    Never,
}

/// Stdout logging configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StdoutLogConfig {
    /// Output format for stdout logs.
    pub format: LogFormat,
}

impl Default for StdoutLogConfig {
    fn default() -> Self {
        Self { format: LogFormat::Full }
    }
}

/// File logging configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLogConfig {
    /// Directory path for log files.
    pub directory_path: PathBuf,
    /// Output format for file logs.
    pub format: LogFormat,
    /// Log rotation strategy.
    pub rotation: LogRotation,
}

/// Complete logging configuration.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Global log level filter.
    pub global_level: LevelFilter,
    /// Stdout logging config (None = disabled).
    pub stdout_logs: Option<StdoutLogConfig>,
    /// File logging config (None = disabled).
    pub file_logs: Option<FileLogConfig>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            global_level: LevelFilter::INFO,
            stdout_logs: Some(StdoutLogConfig::default()),
            file_logs: None,
        }
    }
}

/// Converts verbosity count (1-5) to [`LevelFilter`].
///
/// - 1 = ERROR
/// - 2 = WARN
/// - 3 = INFO (default)
/// - 4 = DEBUG
/// - 5+ = TRACE
#[inline]
pub const fn verbosity_to_level_filter(verbosity: u8) -> LevelFilter {
    match verbosity {
        1 => LevelFilter::ERROR,
        2 => LevelFilter::WARN,
        3 => LevelFilter::INFO,
        4 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::error(1, LevelFilter::ERROR)]
    #[case::warn(2, LevelFilter::WARN)]
    #[case::info(3, LevelFilter::INFO)]
    #[case::debug(4, LevelFilter::DEBUG)]
    #[case::trace(5, LevelFilter::TRACE)]
    #[case::saturates_high(10, LevelFilter::TRACE)]
    fn verbosity_mapping(#[case] level: u8, #[case] expected: LevelFilter) {
        assert_eq!(verbosity_to_level_filter(level), expected);
    }

    #[rstest]
    #[case::full(LogFormat::Full, "full")]
    #[case::json(LogFormat::Json, "json")]
    #[case::pretty(LogFormat::Pretty, "pretty")]
    #[case::compact(LogFormat::Compact, "compact")]
    #[case::logfmt(LogFormat::Logfmt, "logfmt")]
    fn log_format_serde(#[case] format: LogFormat, #[case] str_val: &str) {
        let json = serde_json::to_string(&format).unwrap();
        assert!(json.contains(str_val));
        let parsed: LogFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, format);
    }

    #[rstest]
    #[case::minutely(LogRotation::Minutely, "minutely")]
    #[case::hourly(LogRotation::Hourly, "hourly")]
    #[case::daily(LogRotation::Daily, "daily")]
    #[case::never(LogRotation::Never, "never")]
    fn log_rotation_serde(#[case] rotation: LogRotation, #[case] str_val: &str) {
        let json = serde_json::to_string(&rotation).unwrap();
        assert!(json.contains(str_val));
        let parsed: LogRotation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rotation);
    }

    #[test]
    fn log_format_default_is_full() {
        assert_eq!(LogFormat::default(), LogFormat::Full);
    }

    #[test]
    fn log_rotation_default_is_never() {
        assert_eq!(LogRotation::default(), LogRotation::Never);
    }

    #[test]
    fn log_config_default() {
        let cfg = LogConfig::default();
        assert_eq!(cfg.global_level, LevelFilter::INFO);
        assert!(cfg.stdout_logs.is_some());
        assert!(cfg.file_logs.is_none());
    }
}
