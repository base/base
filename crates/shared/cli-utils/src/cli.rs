//! CLI parsing utilities.

/// Parses CLI arguments with package version and description from the calling crate.
///
/// This macro customizes the CLI with the binary's package name, version, and description
/// from `Cargo.toml`, ensuring `--version` and `--help` show the correct information.
///
/// # Example
///
/// ```ignore
/// use reth_optimism_cli::Cli;
///
/// // Basic usage
/// let cli = base_cli_utils::parse_cli!(Cli<ChainSpecParser, Args>);
///
/// // With additional command customization
/// let cli = base_cli_utils::parse_cli!(Cli<ChainSpecParser, Args>, |cmd| {
///     cmd.mut_arg("some_arg", |arg| arg.default_value("value"))
/// });
/// ```
#[macro_export]
macro_rules! parse_cli {
    ($cli_type:ty) => {{ $crate::parse_cli!($cli_type, |cmd| cmd) }};
    ($cli_type:ty, $customize:expr) => {{
        use $crate::clap::{CommandFactory, FromArgMatches};

        let pkg_name = env!("CARGO_PKG_NAME");
        let version = env!("CARGO_PKG_VERSION");
        let description = env!("CARGO_PKG_DESCRIPTION");

        let cmd = <$cli_type>::command()
            .version(version)
            .long_version(version)
            .about(description)
            .name(pkg_name);
        let cmd = ($customize)(cmd);
        let matches = cmd.get_matches();
        <$cli_type>::from_arg_matches(&matches).expect("Parsing args")
    }};
}
