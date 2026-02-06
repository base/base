//! Global CLI arguments for the Base stack.

use std::path::PathBuf;

use alloy_primitives::Address;
use clap::{ArgAction, Parser};
use kona_registry::OPCHAINS;

use crate::{
    FileLogConfig, LogConfig, LogFormat, LogRotation, MetricsArgs, StdoutLogConfig,
    verbosity_to_level_filter,
};

/// Log-related CLI arguments.
///
/// Verbosity levels: 1=ERROR, 2=WARN, 3=INFO (default), 4=DEBUG, 5=TRACE.
/// Use `-q` to suppress stdout logging entirely.
#[derive(Debug, Clone, Parser)]
pub struct LogArgs {
    /// Increase logging verbosity (1=ERROR, 2=WARN, 3=INFO, 4=DEBUG, 5=TRACE).
    #[arg(
        short = 'v',
        long = "verbose",
        action = ArgAction::Count,
        default_value = "3",
        env = "BASE_NODE_LOG_LEVEL",
        global = true
    )]
    pub level: u8,

    /// Suppress stdout logging.
    #[arg(long = "quiet", short = 'q', global = true)]
    pub stdout_quiet: bool,

    /// Stdout log format.
    #[arg(
        long = "log-format",
        default_value = "full",
        env = "BASE_NODE_LOG_FORMAT",
        global = true
    )]
    pub stdout_format: LogFormat,

    /// Directory for file logging (enables file logging when set).
    #[arg(long = "log-dir", env = "BASE_NODE_LOG_DIR", global = true)]
    pub file_directory: Option<PathBuf>,

    /// File log format.
    #[arg(long = "log-file-format", default_value = "json", global = true)]
    pub file_format: LogFormat,

    /// File log rotation strategy.
    #[arg(long = "log-rotation", default_value = "never", global = true)]
    pub file_rotation: LogRotation,
}

impl Default for LogArgs {
    fn default() -> Self {
        Self {
            level: 3, // INFO
            stdout_quiet: false,
            stdout_format: LogFormat::Full,
            file_directory: None,
            file_format: LogFormat::Json,
            file_rotation: LogRotation::Never,
        }
    }
}

impl From<LogArgs> for LogConfig {
    fn from(args: LogArgs) -> Self {
        let stdout_logs = if args.stdout_quiet {
            None
        } else {
            Some(StdoutLogConfig { format: args.stdout_format })
        };

        let file_logs = args.file_directory.map(|dir| FileLogConfig {
            directory_path: dir,
            format: args.file_format,
            rotation: args.file_rotation,
        });

        Self { global_level: verbosity_to_level_filter(args.level), stdout_logs, file_logs }
    }
}

/// Global arguments shared across all CLI commands.
///
/// Chain ID defaults to Base Mainnet (8453). Can be set via `--network` or `BASE_NODE_NETWORK` env.
#[derive(Debug, Clone, Parser)]
pub struct GlobalArgs {
    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(
        long = "network",
        alias = "chain-id",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NODE_NETWORK"
    )]
    pub l2_chain_id: alloy_chains::Chain,

    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Prometheus CLI arguments.
    #[command(flatten)]
    pub metrics: MetricsArgs,
}

impl GlobalArgs {
    /// Returns the signer [`Address`] from the rollup config for the given l2 chain id.
    pub fn genesis_signer(&self) -> eyre::Result<Address> {
        let id = self.l2_chain_id;
        OPCHAINS
            .get(&id.id())
            .ok_or_else(|| eyre::eyre!("No chain config found for chain ID: {id}"))?
            .roles
            .as_ref()
            .ok_or_else(|| eyre::eyre!("No roles found for chain ID: {id}"))?
            .unsafe_block_signer
            .ok_or_else(|| eyre::eyre!("No unsafe block signer found for chain ID: {id}"))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use rstest::rstest;
    use tracing::level_filters::LevelFilter;

    use super::*;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        log: LogArgs,
    }

    fn parse_log_args(args: &[&str]) -> LogArgs {
        TestCli::parse_from(std::iter::once("test").chain(args.iter().copied())).log
    }

    #[rstest]
    #[case::default(&[], 3)]
    #[case::single_v(&["-v"], 1)]
    #[case::double_v(&["-vv"], 2)]
    #[case::triple_v(&["-vvv"], 3)]
    #[case::quad_v(&["-vvvv"], 4)]
    #[case::quint_v(&["-vvvvv"], 5)]
    fn verbosity_parsing(#[case] args: &[&str], #[case] expected: u8) {
        // Note: ArgAction::Count counts occurrences. When no -v flags are passed,
        // the default_value of "3" (INFO) is used.
        assert_eq!(parse_log_args(args).level, expected);
    }

    #[test]
    fn quiet_mode_short_flag() {
        assert!(parse_log_args(&["-q"]).stdout_quiet);
    }

    #[test]
    fn quiet_mode_long_flag() {
        assert!(parse_log_args(&["--quiet"]).stdout_quiet);
    }

    #[rstest]
    #[case::full("full", LogFormat::Full)]
    #[case::compact("compact", LogFormat::Compact)]
    #[case::json("json", LogFormat::Json)]
    #[case::pretty("pretty", LogFormat::Pretty)]
    #[case::logfmt("logfmt", LogFormat::Logfmt)]
    fn stdout_format_parsing(#[case] format_str: &str, #[case] expected: LogFormat) {
        assert_eq!(parse_log_args(&["--log-format", format_str]).stdout_format, expected);
    }

    #[test]
    fn log_args_to_config_default() {
        let args = LogArgs::default();
        let config: LogConfig = args.into();

        assert_eq!(config.global_level, LevelFilter::INFO);
        assert!(config.stdout_logs.is_some());
        assert!(config.file_logs.is_none());
    }

    #[test]
    fn log_args_to_config_quiet_disables_stdout() {
        let args = LogArgs { stdout_quiet: true, ..Default::default() };
        let config: LogConfig = args.into();

        assert!(config.stdout_logs.is_none());
    }

    #[test]
    fn log_args_to_config_with_file_logging() {
        let args = LogArgs {
            file_directory: Some("/var/log/test".into()),
            file_format: LogFormat::Json,
            file_rotation: LogRotation::Hourly,
            ..Default::default()
        };
        let config: LogConfig = args.into();

        let file_config = config.file_logs.expect("file_logs should be Some");
        assert_eq!(file_config.directory_path, std::path::PathBuf::from("/var/log/test"));
        assert_eq!(file_config.format, LogFormat::Json);
        assert_eq!(file_config.rotation, LogRotation::Hourly);
    }

    #[test]
    fn global_args_default_chain() {
        #[derive(Parser)]
        struct GlobalCli {
            #[command(flatten)]
            global: GlobalArgs,
        }

        let cli = GlobalCli::parse_from(["test"]);
        assert_eq!(cli.global.l2_chain_id.id(), 8453); // Base Mainnet
    }

    #[rstest]
    #[case::base_mainnet("8453", 8453)]
    #[case::base_sepolia("84532", 84532)]
    fn chain_id_parsing(#[case] chain_str: &str, #[case] expected_id: u64) {
        #[derive(Parser)]
        struct GlobalCli {
            #[command(flatten)]
            global: GlobalArgs,
        }

        let cli = GlobalCli::parse_from(["test", "--network", chain_str]);
        assert_eq!(cli.global.l2_chain_id.id(), expected_id);
    }

    #[test]
    fn network_name_parsing() {
        #[derive(Parser)]
        struct GlobalCli {
            #[command(flatten)]
            global: GlobalArgs,
        }

        let cli = GlobalCli::parse_from(["test", "--network", "base"]);
        assert_eq!(cli.global.l2_chain_id.id(), 8453); // Base Mainnet
    }

    /// Tests for environment variable parsing.
    ///
    /// These tests verify that the `BASE_NODE_` prefixed env vars work correctly.
    /// We test using CLI args (which clap parses the same way) to avoid test
    /// isolation issues with global environment variables.
    mod env_vars {
        use super::*;

        /// Verify that `LogArgs` fields have the expected env var names.
        /// This is done by checking the clap metadata.
        #[test]
        fn log_args_env_var_names() {
            use clap::CommandFactory;

            let cmd = TestCli::command();
            let args: Vec<_> = cmd.get_arguments().collect();

            // Find the verbose arg and check its env var
            let verbose_arg = args.iter().find(|a| a.get_long() == Some("verbose"));
            assert!(verbose_arg.is_some(), "verbose arg should exist");
            assert_eq!(
                verbose_arg.unwrap().get_env().map(|s| s.to_str().unwrap()),
                Some("BASE_NODE_LOG_LEVEL"),
                "verbose should use BASE_NODE_LOG_LEVEL env var"
            );

            // Find the log-format arg and check its env var
            let format_arg = args.iter().find(|a| a.get_long() == Some("log-format"));
            assert!(format_arg.is_some(), "log-format arg should exist");
            assert_eq!(
                format_arg.unwrap().get_env().map(|s| s.to_str().unwrap()),
                Some("BASE_NODE_LOG_FORMAT"),
                "log-format should use BASE_NODE_LOG_FORMAT env var"
            );

            // Find the log-dir arg and check its env var
            let dir_arg = args.iter().find(|a| a.get_long() == Some("log-dir"));
            assert!(dir_arg.is_some(), "log-dir arg should exist");
            assert_eq!(
                dir_arg.unwrap().get_env().map(|s| s.to_str().unwrap()),
                Some("BASE_NODE_LOG_DIR"),
                "log-dir should use BASE_NODE_LOG_DIR env var"
            );
        }
    }
}
