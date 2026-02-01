use std::process::Command;

use eyre::{Result, ensure};

const NETWORK_NAME: &str = "system-tests-network";

/// Returns the name of the Docker network used for system tests.
pub const fn network_name() -> &'static str {
    NETWORK_NAME
}

/// Ensures that the Docker network exists.
pub fn ensure_network_exists() -> Result<()> {
    let exists = Command::new("docker")
        .args(["network", "inspect", NETWORK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if exists {
        return Ok(());
    }

    let status = Command::new("docker").args(["network", "create", NETWORK_NAME]).status()?;

    ensure!(status.success(), "Failed to create Docker network");

    Ok(())
}

/// Removes the Docker network.
pub fn cleanup_network() {
    let _ = Command::new("docker").args(["network", "rm", NETWORK_NAME]).output();
}
