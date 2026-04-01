//! Helpers for invoking external command-line tooling.

use std::{
    path::Path,
    process::{Command, Stdio},
};

use eyre::{Result, WrapErr, ensure};

/// Runs a command and returns an error when it exits unsuccessfully.
pub(crate) fn run_command(command: &mut Command, purpose: &str) -> Result<()> {
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let output = command.output().wrap_err_with(|| format!("Failed to {purpose}"))?;
    ensure!(
        output.status.success(),
        "{purpose} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Captures a command's stdout into a file.
pub(crate) fn capture_stdout_to_path(
    command: &mut Command,
    path: &Path,
    purpose: &str,
) -> Result<()> {
    let output = command.output().wrap_err_with(|| format!("Failed to {purpose}"))?;
    ensure!(
        output.status.success(),
        "{purpose} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    std::fs::write(path, output.stdout)
        .wrap_err_with(|| format!("Failed to write {}", path.display()))
}
