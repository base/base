//! Logging configuration utilities for CLI applications.
//!
//! This module provides structured logging configuration with support for:
//! - Verbosity levels (`-v`, `-vv`, `-vvv`)
//! - Multiple output formats (full, compact, json)
//! - Optional file logging with rotation capability
//!
//! # Example
//!
//! ```rust,ignore
//! use clap::Parser;
//! use base_cli_utils::LoggingArgs;
//!
//! #[derive(Parser)]
//! struct Cli {
//!     #[command(flatten)]
//!     logging: LoggingArgs,
//! }
//!
//! let cli = Cli::parse();
//! let level = cli.logging.log_level();
//! ```

use std::path::PathBuf;

use clap::{ArgAction, Parser, ValueEnum};

/// Log output format.
///
/// Determines how log messages are formatted when written to stdout/stderr or files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Full format with all metadata (timestamp, level, target, spans).
    ///
    /// Example: `2024-01-15T10:30:00.123456Z INFO node_reth::sync: Starting sync`
    #[default]
    Full,

    /// Compact format with minimal metadata.
    ///
    /// Example: `INFO sync: Starting sync`
    Compact,

    /// JSON format for structured logging and log aggregation systems.
    ///
    /// Example: `{"timestamp":"2024-01-15T10:30:00.123456Z","level":"INFO","message":"Starting sync"}`
    Json,
}

/// Log rotation strategy for file logging.
///
/// Specifies when log files should be rotated to prevent unbounded growth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogRotation {
    /// Rotate logs every minute (primarily for testing).
    Minutely,

    /// Rotate logs every hour.
    #[default]
    Hourly,

    /// Rotate logs every day at midnight.
    Daily,

    /// Never rotate logs (single file that grows indefinitely).
    Never,
}

/// Logging configuration arguments for CLI applications.
///
/// Provides a standardized way to configure logging across all node-reth CLI tools.
/// Can be embedded into other argument structs using clap's `#[command(flatten)]`.
///
/// # Verbosity Levels
///
/// The verbosity flag controls the log level:
/// - No flag: `WARN` level
/// - `-v`: `INFO` level
/// - `-vv`: `DEBUG` level
/// - `-vvv` or more: `TRACE` level
///
/// # Examples
///
/// Basic usage with clap:
///
/// ```rust,ignore
/// use clap::Parser;
/// use base_cli_utils::LoggingArgs;
///
/// #[derive(Parser)]
/// struct MyCli {
///     #[command(flatten)]
///     logging: LoggingArgs,
/// }
/// ```
///
/// Command line examples:
///
/// ```text
/// # Default (WARN level, full format)
/// my-app
///
/// # INFO level with compact format
/// my-app -v --log-format compact
///
/// # DEBUG level with JSON format and file logging
/// my-app -vv --log-format json --log-file /var/log/my-app.log
///
/// # TRACE level with daily log rotation
/// my-app -vvv --log-file /var/log/my-app.log --log-rotation daily
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Parser)]
pub struct LoggingArgs {
    /// Increase logging verbosity.
    ///
    /// Use multiple times for more verbose output:
    /// - `-v`: INFO level
    /// - `-vv`: DEBUG level
    /// - `-vvv`: TRACE level
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count, global = true)]
    pub verbosity: u8,

    /// Log output format.
    ///
    /// Controls how log messages are formatted:
    /// - `full`: Complete format with timestamp, level, target, and spans
    /// - `compact`: Minimal format with just level and message
    /// - `json`: Structured JSON format for log aggregation
    #[arg(long = "log-format", default_value = "full", global = true)]
    pub format: LogFormat,

    /// Path to write log output to a file.
    ///
    /// When specified, logs will be written to this file in addition to stdout.
    /// The parent directory must exist.
    #[arg(long = "log-file", global = true)]
    pub log_file: Option<PathBuf>,

    /// Log file rotation strategy.
    ///
    /// Only applicable when `--log-file` is specified:
    /// - `minutely`: Rotate every minute (for testing)
    /// - `hourly`: Rotate every hour
    /// - `daily`: Rotate every day at midnight
    /// - `never`: Never rotate (single growing file)
    #[arg(long = "log-rotation", global = true)]
    pub log_rotation: Option<LogRotation>,
}

impl LoggingArgs {
    /// Converts the verbosity count to a [`tracing::Level`].
    ///
    /// The mapping is:
    /// - `0`: [`tracing::Level::WARN`]
    /// - `1`: [`tracing::Level::INFO`]
    /// - `2`: [`tracing::Level::DEBUG`]
    /// - `3+`: [`tracing::Level::TRACE`]
    ///
    /// # Examples
    ///
    /// ```rust
    /// use base_cli_utils::LoggingArgs;
    ///
    /// let args = LoggingArgs::default();
    /// assert_eq!(args.log_level(), tracing::Level::WARN);
    ///
    /// let args = LoggingArgs { verbosity: 2, ..Default::default() };
    /// assert_eq!(args.log_level(), tracing::Level::DEBUG);
    /// ```
    #[inline]
    pub const fn log_level(&self) -> tracing::Level {
        match self.verbosity {
            0 => tracing::Level::WARN,
            1 => tracing::Level::INFO,
            2 => tracing::Level::DEBUG,
            _ => tracing::Level::TRACE,
        }
    }

    /// Converts the verbosity count to a [`tracing::level_filters::LevelFilter`].
    ///
    /// This is useful when configuring tracing subscribers that require a filter.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use base_cli_utils::LoggingArgs;
    /// use tracing::level_filters::LevelFilter;
    ///
    /// let args = LoggingArgs::default();
    /// assert_eq!(args.log_level_filter(), LevelFilter::WARN);
    /// ```
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
    ///
    /// # Examples
    ///
    /// ```rust
    /// use base_cli_utils::LoggingArgs;
    /// use std::path::PathBuf;
    ///
    /// let args = LoggingArgs::default();
    /// assert!(!args.has_file_logging());
    ///
    /// let args = LoggingArgs {
    ///     log_file: Some(PathBuf::from("/var/log/app.log")),
    ///     ..Default::default()
    /// };
    /// assert!(args.has_file_logging());
    /// ```
    #[inline]
    pub const fn has_file_logging(&self) -> bool {
        self.log_file.is_some()
    }

    /// Returns the configured log rotation strategy.
    ///
    /// If no rotation is explicitly configured, returns [`LogRotation::Hourly`] as the default.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use base_cli_utils::{LoggingArgs, LogRotation};
    ///
    /// let args = LoggingArgs::default();
    /// assert_eq!(args.rotation(), LogRotation::Hourly);
    ///
    /// let args = LoggingArgs {
    ///     log_rotation: Some(LogRotation::Daily),
    ///     ..Default::default()
    /// };
    /// assert_eq!(args.rotation(), LogRotation::Daily);
    /// ```
    #[inline]
    pub fn rotation(&self) -> LogRotation {
        self.log_rotation.unwrap_or_default()
    }

    /// Returns `true` if the log format is JSON.
    ///
    /// This is useful for conditional configuration of tracing subscribers.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use base_cli_utils::{LoggingArgs, LogFormat};
    ///
    /// let args = LoggingArgs::default();
    /// assert!(!args.is_json_format());
    ///
    /// let args = LoggingArgs {
    ///     format: LogFormat::Json,
    ///     ..Default::default()
    /// };
    /// assert!(args.is_json_format());
    /// ```
    #[inline]
    pub const fn is_json_format(&self) -> bool {
        matches!(self.format, LogFormat::Json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_log_level() {
        let args = LoggingArgs::default();
        assert_eq!(args.log_level(), tracing::Level::WARN);
        assert_eq!(args.log_level_filter(), tracing::level_filters::LevelFilter::WARN);
    }

    #[test]
    fn test_verbosity_levels() {
        let args = LoggingArgs { verbosity: 1, ..Default::default() };
        assert_eq!(args.log_level(), tracing::Level::INFO);

        let args = LoggingArgs { verbosity: 2, ..Default::default() };
        assert_eq!(args.log_level(), tracing::Level::DEBUG);

        let args = LoggingArgs { verbosity: 3, ..Default::default() };
        assert_eq!(args.log_level(), tracing::Level::TRACE);

        // Values above 3 should still be TRACE
        let args = LoggingArgs { verbosity: 10, ..Default::default() };
        assert_eq!(args.log_level(), tracing::Level::TRACE);
    }

    #[test]
    fn test_default_format() {
        let args = LoggingArgs::default();
        assert_eq!(args.format, LogFormat::Full);
        assert!(!args.is_json_format());
    }

    #[test]
    fn test_json_format() {
        let args = LoggingArgs { format: LogFormat::Json, ..Default::default() };
        assert!(args.is_json_format());
    }

    #[test]
    fn test_file_logging() {
        let args = LoggingArgs::default();
        assert!(!args.has_file_logging());

        let args =
            LoggingArgs { log_file: Some(PathBuf::from("/tmp/test.log")), ..Default::default() };
        assert!(args.has_file_logging());
    }

    #[test]
    fn test_log_rotation() {
        let args = LoggingArgs::default();
        assert_eq!(args.rotation(), LogRotation::Hourly);

        let args = LoggingArgs { log_rotation: Some(LogRotation::Daily), ..Default::default() };
        assert_eq!(args.rotation(), LogRotation::Daily);
    }
}
