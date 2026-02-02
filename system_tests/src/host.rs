//! Host connectivity utilities for container-to-host communication.
//!
//! This module provides platform-aware abstractions for containers to reach
//! services running on the host machine (e.g., in-process builder/client nodes).
//!
//! # Platform Behavior
//!
//! - **macOS**: Uses `host.docker.internal` DNS name provided by Docker Desktop.
//!   This avoids spawning SSHD containers which clutter Docker Desktop during local dev.
//!
//! - **Linux**: Uses testcontainers' `with_exposed_host_port()` which spawns an SSHD
//!   container to tunnel host ports into the Docker network. This is required in CI
//!   environments where Docker Desktop is not available.
//!
//! # Usage
//!
//! Instead of:
//! ```ignore
//! container_builder
//!     .with_exposed_host_port(port)
//!     ...
//! ```
//!
//! Use:
//! ```ignore
//! use system_tests::host::{host_address, with_host_port_if_needed};
//!
//! // Get the host address to use in container args
//! let engine_url = format!("http://{}:{}", host_address(), port);
//!
//! // Conditionally expose host port (only on Linux)
//! let container_builder = with_host_port_if_needed(container_builder, port);
//! ```

use testcontainers::{ContainerRequest, Image, ImageExt};

/// Returns the address containers should use to reach the host machine.
///
/// - On macOS: Returns `host.docker.internal` (Docker Desktop built-in)
/// - On Linux: Returns `host.docker.internal` (works with testcontainers SSHD)
///
/// Note: On Linux, you must also call `with_host_port_if_needed()` to expose
/// the port via testcontainers' SSHD tunnel.
pub const fn host_address() -> &'static str {
    "host.docker.internal"
}

/// Conditionally exposes a host port to the container.
///
/// - On macOS: Adds `host.docker.internal` -> `host-gateway` mapping
/// - On Linux: Calls `with_exposed_host_port()` to enable SSHD tunnel
///
/// This prevents SSHD container spam on macOS during local development while
/// maintaining full functionality in Linux CI environments.
#[cfg(target_os = "macos")]
pub fn with_host_port_if_needed<I: Image>(
    container: ContainerRequest<I>,
    _port: u16,
) -> ContainerRequest<I> {
    container.with_host("host.docker.internal", Host::HostGateway)
}

/// Conditionally exposes a host port to the container.
///
/// - On macOS: Adds `host.docker.internal` -> `host-gateway` mapping
/// - On Linux: Calls `with_exposed_host_port()` to enable SSHD tunnel
///
/// This prevents SSHD container spam on macOS during local development while
/// maintaining full functionality in Linux CI environments.
#[cfg(not(target_os = "macos"))]
pub fn with_host_port_if_needed<I: Image>(
    container: ContainerRequest<I>,
    port: u16,
) -> ContainerRequest<I> {
    container.with_exposed_host_port(port)
}
