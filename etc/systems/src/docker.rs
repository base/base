//! Docker utility functions for system test container management.

use std::process::Command;

use eyre::{Result, ensure};

use crate::{
    containers::{L1_BEACON_NAME, L1_RETH_NAME, L1_VALIDATOR_NAME},
    network_name,
};

const SYSTEM_TEST_CONTAINER_NAMES: &[&str] = &[L1_RETH_NAME, L1_BEACON_NAME, L1_VALIDATOR_NAME];

/// Lists all system test containers (running or stopped).
pub fn list_system_test_containers() -> Result<Vec<String>> {
    let mut containers = Vec::new();

    for name in SYSTEM_TEST_CONTAINER_NAMES {
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

/// Returns true if any system test containers are running.
pub fn is_system_test_running() -> Result<bool> {
    for name in SYSTEM_TEST_CONTAINER_NAMES {
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

/// Stops and removes all system test containers.
pub fn stop_system_test_containers() -> Result<()> {
    let containers = list_system_test_containers()?;

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

/// Removes the system test Docker network.
pub fn cleanup_system_test_network() -> Result<()> {
    let output = Command::new("docker").args(["network", "rm", network_name()]).output()?;

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
