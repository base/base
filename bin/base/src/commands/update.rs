//! Base binary update command.

use std::{ffi::OsString, path::Path, process::Command};

use clap::Args;
use eyre::{OptionExt, WrapErr};

/// Arguments for `base update`.
#[derive(Args, Clone, Debug)]
pub(crate) struct UpdateCommand {
    /// Install a specific release tag instead of the latest release.
    #[arg(short = 'i', long = "install", value_name = "VER")]
    pub(crate) version: Option<String>,

    /// Update the baseup installer instead of the base binary.
    #[arg(long, conflicts_with = "version")]
    pub(crate) update_installer: bool,

    /// Skip release signature and attestation verification.
    #[arg(long)]
    pub(crate) unsafe_skip_verify: bool,
}

impl UpdateCommand {
    /// Updates the `base` binary by delegating release fetch and verification to `baseup`.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let bin_dir = std::env::current_exe()
            .wrap_err("failed to locate current base executable")?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_eyre("failed to locate current base executable directory")?;

        let mut command = Command::new(Self::baseup_path(&bin_dir));
        command.env("BASE_BIN_DIR", &bin_dir);

        if self.update_installer {
            command.arg("--update");
        } else {
            command.args(["--bin", "base"]);
            if let Some(version) = self.version {
                command.arg("--install").arg(version);
            }
        }

        if self.unsafe_skip_verify {
            command.arg("--unsafe-skip-verify");
        }

        let status = command
            .status()
            .wrap_err("failed to execute baseup; install it with the baseup bootstrap first")?;

        if !status.success() {
            eyre::bail!("baseup exited with status {status}");
        }

        Ok(())
    }

    fn baseup_path(bin_dir: &Path) -> OsString {
        let sibling = bin_dir.join("baseup");
        if sibling.is_file() { sibling.into_os_string() } else { OsString::from("baseup") }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{cli::BaseCli, commands::BaseCommand};

    #[test]
    fn parses_update_command() {
        let cli = BaseCli::parse_from(["base", "update"]);

        assert!(matches!(cli.command, BaseCommand::Update(_)));
    }

    #[test]
    fn parses_update_command_with_version() {
        let cli = BaseCli::parse_from(["base", "update", "--install", "v0.6.0"]);
        let BaseCommand::Update(update) = cli.command else {
            panic!("expected update command");
        };

        assert_eq!(update.version.as_deref(), Some("v0.6.0"));
    }

    #[test]
    fn parses_update_installer_command() {
        let cli = BaseCli::parse_from(["base", "update", "--update-installer"]);
        let BaseCommand::Update(update) = cli.command else {
            panic!("expected update command");
        };

        assert!(update.update_installer);
    }

    #[test]
    fn rejects_update_installer_with_version() {
        let err = BaseCli::try_parse_from([
            "base",
            "update",
            "--update-installer",
            "--install",
            "v0.6.0",
        ])
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("cannot be used with"));
    }
}
