//! CLI command to show configs.

use std::path::PathBuf;

use clap::Parser;
use eyre::{WrapErr, bail};
use reth_config::Config;
/// `reth config` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    config: Option<PathBuf>,

    /// Show the default config
    #[arg(long, verbatim_doc_comment, conflicts_with = "config")]
    default: bool,
}

impl Command {
    /// Execute `config` command
    pub async fn execute(&self) -> eyre::Result<()> {
        let config = if self.default {
            Config::default()
        } else {
            let path = match self.config.as_ref() {
                Some(path) => path,
                None => {
                    bail!("No config file provided. Use --config <FILE> or pass --default");
                }
            };
            if !path.exists() {
                bail!("Config file does not exist: {}", path.display());
            }
            Config::from_path(path)
                .wrap_err_with(|| format!("Could not load config file: {}", path.display()))?
        };
        println!("{}", toml_0_9_12_spec_1_1_0::to_string_pretty(&config)?);
        Ok(())
    }
}
