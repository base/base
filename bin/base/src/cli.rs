use base_cli_utils::{LogConfig, MetricsConfig};
use clap::Parser;
use eyre::WrapErr;

use crate::{
    commands::BaseCommand,
    config::{ChainArg, ChainResolver},
};

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);

/// The `base` CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    styles = base_cli_utils::CliStyles::init(),
    about,
    long_about = None
)]
pub(crate) struct BaseCli {
    /// Chain selection.
    #[arg(long, short = 'c', global = true, default_value = "mainnet", env = "BASE_CHAIN")]
    pub(crate) chain: ChainArg,

    /// Logging configuration.
    #[command(flatten)]
    pub(crate) logging: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub(crate) metrics: MetricsArgs,

    /// The command to run.
    #[command(subcommand)]
    pub(crate) command: BaseCommand,
}

impl BaseCli {
    /// Runs the selected command with shared process initialization.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { chain, logging, metrics, command } = self;

        LogConfig::from(logging)
            .init_tracing_subscriber()
            .wrap_err("failed to initialize tracing")?;

        let metrics_enabled = metrics.enabled;
        MetricsConfig::from(metrics)
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
            })
            .wrap_err("failed to install Prometheus recorder")?;

        command.run(ChainResolver::new(chain), metrics_enabled)
    }
}
#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::config::BuiltInChain;

    #[test]
    fn parses_default_chain_for_rpc() {
        let cli = BaseCli::parse_from([
            "base",
            "rpc",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
        ]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Mainnet)));
        assert!(matches!(cli.command, BaseCommand::Rpc(_)));
    }

    #[test]
    fn parses_named_chain_selector() {
        let cli = BaseCli::parse_from(["base", "-c", "sepolia", "bootnode"]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Sepolia)));
    }

    #[test]
    fn parses_global_chain_after_subcommand() {
        let cli = BaseCli::parse_from(["base", "bootnode", "--chain", "sepolia"]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Sepolia)));
    }

    #[test]
    fn parses_path_chain_selector() {
        let cli = BaseCli::parse_from(["base", "--chain", "./chain.toml", "bootnode"]);

        assert!(matches!(cli.chain, ChainArg::File(_)));
    }

    #[test]
    fn chain_arg_uses_base_chain_env_var() {
        let command = BaseCli::command();
        let chain_arg =
            command.get_arguments().find(|arg| arg.get_long() == Some("chain")).unwrap();

        assert_eq!(chain_arg.get_env(), Some(OsStr::new("BASE_CHAIN")));
    }

    #[test]
    fn rejects_multiple_chain_selectors() {
        let err =
            BaseCli::try_parse_from(["base", "-c", "mainnet", "--chain", "sepolia", "bootnode"])
                .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("cannot be used multiple times"));
    }
}
