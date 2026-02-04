//! Docker utility functions for devnet container management.

use std::process::Command;

use eyre::{Result, ensure};

use crate::containers::{
    L1_BEACON_NAME, L1_RETH_NAME, L1_VALIDATOR_NAME, L2_BATCHER_NAME, L2_CLIENT_OP_NODE_NAME,
    L2_OP_NODE_NAME,
};

/// Docker network name for devnet.
pub const DEVNET_NETWORK: &str = "devnet-network";

const DEVNET_CONTAINER_NAMES: &[&str] = &[
    L1_RETH_NAME,
    L1_BEACON_NAME,
    L1_VALIDATOR_NAME,
    L2_OP_NODE_NAME,
    L2_BATCHER_NAME,
    L2_CLIENT_OP_NODE_NAME,
];

/// Lists all devnet containers (running or stopped).
pub fn list_devnet_containers() -> Result<Vec<String>> {
    let mut containers = Vec::new();

    for name in DEVNET_CONTAINER_NAMES {
        let output = Command::new("docker")
            .args(["ps", "-a", "--filter", &format!("name=^/{name}$"), "--format", "{{.Names}}"])
            .output()?;

        if output.status.success() {
            let names = String::from_utf8_lossy(&output.stdout);
            for line in names.lines() {
                if !line.is_empty() {
                    containers.push(line.to_string());
                }
            }
        }
    }

    Ok(containers)
}

/// Returns true if any devnet containers are running.
pub fn is_devnet_running() -> Result<bool> {
    for name in DEVNET_CONTAINER_NAMES {
        let output = Command::new("docker")
            .args(["ps", "--filter", &format!("name=^/{name}$"), "--format", "{{.Names}}"])
            .output()?;

        if output.status.success() {
            let names = String::from_utf8_lossy(&output.stdout);
            if names.lines().any(|line| !line.is_empty()) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Stops and removes all devnet containers.
pub fn stop_devnet_containers() -> Result<()> {
    let containers = list_devnet_containers()?;

    for container in containers {
        let stop_output = Command::new("docker").args(["stop", &container]).output()?;

        if !stop_output.status.success() {
            let stderr = String::from_utf8_lossy(&stop_output.stderr);
            if !stderr.contains("is not running") && !stderr.contains("No such container") {
                ensure!(
                    stop_output.status.success(),
                    "Failed to stop container {}: {}",
                    container,
                    stderr
                );
            }
        }

        let rm_output = Command::new("docker").args(["rm", "-f", &container]).output()?;

        if !rm_output.status.success() {
            let stderr = String::from_utf8_lossy(&rm_output.stderr);
            if !stderr.contains("No such container") {
                ensure!(
                    rm_output.status.success(),
                    "Failed to remove container {}: {}",
                    container,
                    stderr
                );
            }
        }
    }

    Ok(())
}

/// Removes the devnet-network Docker network.
pub fn cleanup_devnet_network() -> Result<()> {
    let output = Command::new("docker").args(["network", "rm", DEVNET_NETWORK]).output()?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such network") {
        return Ok(());
    }

    ensure!(output.status.success(), "Failed to remove Docker network: {}", stderr);

    Ok(())
}
