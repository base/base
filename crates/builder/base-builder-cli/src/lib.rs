#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use clap_builder::{CommandFactory, FromArgMatches};
use reth_optimism_cli::chainspec::OpChainSpecParser;

mod flashblocks;
pub use flashblocks::FlashblocksArgs;

mod op;
pub use op::OpRbuilderArgs;

mod telemetry;
pub use telemetry::TelemetryArgs;

/// CLI type alias for the OP builder.
pub type Cli = reth_optimism_cli::Cli<OpChainSpecParser, OpRbuilderArgs>;

/// Version information for CLI configuration.
#[derive(Debug)]
pub struct CliVersionInfo {
    /// Short version string (e.g., "1.0.0 (abc123)")
    pub short_version: &'static str,
    /// Long version string with detailed build info
    pub long_version: &'static str,
    /// About description for the CLI
    pub about: &'static str,
    /// Author of the CLI
    pub author: &'static str,
    /// Name of the CLI binary
    pub name: &'static str,
    /// Default logs directory
    pub logs_dir: std::ffi::OsString,
}

/// This trait is used to extend Reth's CLI with additional functionality that
/// are specific to the OP builder.
pub trait CliExt {
    /// Returns the Cli instance with the parsed command line arguments.
    fn parsed(version_info: CliVersionInfo) -> Self;

    /// Returns the Cli instance with the parsed command line arguments
    /// and replaces version, name, author, and about
    fn set_version(version_info: CliVersionInfo) -> Self;
}

impl CliExt for Cli {
    fn parsed(version_info: CliVersionInfo) -> Self {
        Self::set_version(version_info)
    }

    /// Parses commands and overrides versions
    fn set_version(version_info: CliVersionInfo) -> Self {
        let matches = Self::command()
            .version(version_info.short_version)
            .long_version(version_info.long_version)
            .about(version_info.about)
            .author(version_info.author)
            .name(version_info.name)
            .mut_arg("log_file_directory", |arg| arg.default_value(version_info.logs_dir))
            .get_matches();
        Self::from_arg_matches(&matches).expect("Parsing args")
    }
}
