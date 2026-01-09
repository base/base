//! Logging configuration for CLI applications.

use std::path::PathBuf;

use clap::{ArgAction, Parser, ValueEnum};

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Full format with timestamp, level, target, and spans.
    #[default]
    Full,
    /// Compact format with minimal metadata.
    Compact,
    /// JSON format for structured logging.
    Json,
}

/// Log rotation strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogRotation {
    /// Rotate every minute (for testing).
    Minutely,
    /// Rotate every hour.
    #[default]
    Hourly,
    /// Rotate every day.
    Daily,
    /// Never rotate.
    Never,
}

/// Logging configuration arguments.
///
/// Verbosity: `-v` (INFO), `-vv` (DEBUG), `-vvv` (TRACE). Default is WARN.
#[derive(Debug, Clone, Default, PartialEq, Eq, Parser)]
pub struct LoggingArgs {
    /// Increase logging verbosity (-v, -vv, -vvv).
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    pub verbosity: u8,

    /// Log output format (full, compact, json).
    #[arg(long = "log-format", default_value = "full", global = true)]
    pub format: LogFormat,

    /// Path to write logs to a file.
    #[arg(long = "log-file", global = true)]
    pub log_file: Option<PathBuf>,

    /// Log file rotation (minutely, hourly, daily, never).
    #[arg(long = "log-rotation", global = true)]
    pub log_rotation: Option<LogRotation>,
}

impl LoggingArgs {
    /// Converts verbosity to a [`tracing::Level`].
    #[inline]
    pub const fn log_level(&self) -> tracing::Level {
        match self.verbosity {
            0 => tracing::Level::WARN,
            1 => tracing::Level::INFO,
            2 => tracing::Level::DEBUG,
            _ => tracing::Level::TRACE,
        }
    }

    /// Converts verbosity to a [`tracing::level_filters::LevelFilter`].
    #[inline]
    pub const fn log_level_filter(&self) -> tracing::level_filters::LevelFilter {
        match self.verbosity {
            0 => tracing::level_filters::LevelFilter::WARN,
            1 => tracing::level_filters::LevelFilter::INFO,
            2 => tracing::level_filters::LevelFilter::DEBUG,
            _ => tracing::level_filters::LevelFilter::TRACE,
        }
    }

    /// Returns `true` if file logging is enabled.
    #[inline]
    pub const fn has_file_logging(&self) -> bool {
        self.log_file.is_some()
    }

    /// Returns the log rotation strategy, defaulting to [`LogRotation::Hourly`].
    #[inline]
    pub fn rotation(&self) -> LogRotation {
        self.log_rotation.unwrap_or_default()
    }

    /// Returns `true` if the log format is JSON.
    #[inline]
    pub const fn is_json_format(&self) -> bool {
        matches!(self.format, LogFormat::Json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verbosity_levels() {
        assert_eq!(LoggingArgs::default().log_level(), tracing::Level::WARN);
        assert_eq!(
            LoggingArgs { verbosity: 1, ..Default::default() }.log_level(),
            tracing::Level::INFO
        );
        assert_eq!(
            LoggingArgs { verbosity: 2, ..Default::default() }.log_level(),
            tracing::Level::DEBUG
        );
        assert_eq!(
            LoggingArgs { verbosity: 3, ..Default::default() }.log_level(),
            tracing::Level::TRACE
        );
    }

    #[test]
    fn test_format_and_rotation() {
        let args = LoggingArgs::default();
        assert_eq!(args.format, LogFormat::Full);
        assert!(!args.is_json_format());
        assert_eq!(args.rotation(), LogRotation::Hourly);

        let args = LoggingArgs { format: LogFormat::Json, ..Default::default() };
        assert!(args.is_json_format());
    }

    #[test]
    fn test_file_logging() {
        assert!(!LoggingArgs::default().has_file_logging());
        let args =
            LoggingArgs { log_file: Some(PathBuf::from("/tmp/test.log")), ..Default::default() };
        assert!(args.has_file_logging());
    }
}
