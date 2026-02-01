use std::process::Command;

use eyre::{Result, ensure};

const NETWORK_NAME: &str = "system-tests-network";

/// Returns the name of the Docker network used for system tests.
pub const fn network_name() -> &'static str {
    NETWORK_NAME
}

/// Ensures that the Docker network exists.
pub fn ensure_network_exists() -> Result<()> {
    let output = Command::new("docker").args(["network", "create", NETWORK_NAME]).output()?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already exists") {
        return Ok(());
    }

    ensure!(output.status.success(), "Failed to create Docker network: {}", stderr);

    Ok(())
}

/// Removes the Docker network.
pub fn cleanup_network() {
    let _ = Command::new("docker").args(["network", "rm", NETWORK_NAME]).output();
}
