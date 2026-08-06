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
#[derive(Parser, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    styles = base_cli_utils::CliStyles::init(),
    about,
    long_about = None
)]
pub(crate) struct BaseCli {
    /// Chain selection.
    ///
    /// Uses a distinct clap `id` so nested reth-derived subcommands (e.g. `base reth db`) can
    /// register their own globally-propagated `--chain` arg without colliding at value-access
    /// time in [`FromArgMatches`].
    #[arg(id = "base_chain", long = "chain", short = 'c', env = "BASE_CHAIN")]
    pub(crate) chain: Option<ChainArg>,

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
        LogConfig::from(self.logging)
            .init_tracing_subscriber()
            .wrap_err("failed to initialize tracing")?;

        let metrics_enabled = self.metrics.enabled;
        MetricsConfig::from(self.metrics)
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
            })
            .wrap_err("failed to install Prometheus recorder")?;

        self.command.run(ChainResolver::new(self.chain), metrics_enabled)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::{CommandFactory, Parser};

    use super::*;

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

        assert_eq!(cli.chain, None);
        assert!(matches!(cli.command, BaseCommand::Rpc(_)));
    }

    #[test]
    fn parses_named_chain_selector() {
        let cli = BaseCli::parse_from(["base", "-c", "sepolia", "bootnode"]);

        assert!(matches!(cli.chain, Some(ChainArg::BuiltIn(ref name)) if name == "sepolia"));
    }

    #[test]
    fn rejects_chain_after_subcommand() {
        // `--chain` is no longer globally propagated so nested reth subcommands can register
        // their own `--chain` arg without clap `Long option names must be unique` collisions.
        // Callers must supply `--chain` before the subcommand: `base --chain sepolia bootnode`.
        let err = BaseCli::try_parse_from(["base", "bootnode", "--chain", "sepolia"]).unwrap_err();

        assert!(err.to_string().contains("unexpected argument '--chain'"));
    }

    #[test]
    fn parses_path_chain_selector() {
        let cli = BaseCli::parse_from(["base", "--chain", "./chain.toml", "bootnode"]);

        assert!(matches!(cli.chain, Some(ChainArg::File(_))));
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

    #[test]
    fn preserves_base_chain_alongside_reth_subcommand_chain() {
        let cli =
            BaseCli::try_parse_from(["base", "--chain", "sepolia", "reth", "db", "stats"]).unwrap();

        assert!(matches!(cli.chain, Some(ChainArg::BuiltIn(ref name)) if name == "sepolia"));
        assert!(matches!(cli.command, BaseCommand::Reth(_)));
    }
}
