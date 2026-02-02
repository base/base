//! Host connectivity utilities for container-to-host communication.
//!
//! This module provides platform-aware abstractions for containers to reach
//! services running on the host machine (e.g., in-process builder/client nodes).
//!
//! # Platform Behavior
//!
//! - **macOS**: Uses `host.docker.internal` with `host-gateway` mapping.
//!   This avoids spawning SSHD containers which clutter Docker Desktop during local dev.
//!
//! - **Linux**: Uses testcontainers' `with_exposed_host_port()` which spawns an SSHD
//!   container to tunnel host ports into the Docker network. This is required in CI
//!   environments where Docker Desktop is not available. Uses `host.testcontainers.internal`.
//!
//! # Usage
//!
//! ```ignore
//! use system_tests::host::{host_address, with_host_port_if_needed};
//!
//! let engine_url = format!("http://{}:{}", host_address(), port);
//! let container_builder = with_host_port_if_needed(container_builder, port);
//! ```

use testcontainers::{ContainerRequest, Image, ImageExt, core::Host};

/// Returns the address containers should use to reach the host machine.
///
/// - On macOS: Returns `host.docker.internal` (Docker Desktop built-in)
/// - On Linux: Returns `host.testcontainers.internal` (testcontainers SSHD tunnel)
#[cfg(target_os = "macos")]
pub const fn host_address() -> &'static str {
    "host.docker.internal"
}

/// Returns the address containers should use to reach the host machine.
///
/// - On macOS: Returns `host.docker.internal` (Docker Desktop built-in)
/// - On Linux: Returns `host.testcontainers.internal` (testcontainers SSHD tunnel)
#[cfg(not(target_os = "macos"))]
pub const fn host_address() -> &'static str {
    "host.testcontainers.internal"
}

/// Configures container for host connectivity.
///
/// - On macOS: Adds `host.docker.internal` -> `host-gateway` mapping
/// - On Linux: Calls `with_exposed_host_port()` to enable SSHD tunnel
#[cfg(target_os = "macos")]
pub fn with_host_port_if_needed<I: Image>(
    container: ContainerRequest<I>,
    _port: u16,
) -> ContainerRequest<I> {
    container.with_host("host.docker.internal", Host::HostGateway)
}

/// Configures container for host connectivity.
///
/// - On macOS: Adds `host.docker.internal` -> `host-gateway` mapping
/// - On Linux: Calls `with_exposed_host_port()` to enable SSHD tunnel
#[cfg(not(target_os = "macos"))]
pub fn with_host_port_if_needed<I: Image>(
    container: ContainerRequest<I>,
    port: u16,
) -> ContainerRequest<I> {
    container.with_exposed_host_port(port)
}
