//! Test utilities for builder-adjacent crates.

use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    time::Duration,
};

/// Hardcoded builder private key (anvil default key[1]).
pub const BUILDER_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// Hardcoded funded account private key (anvil default key[0]).
pub const FUNDED_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Default block gas limit used in tests.
pub const DEFAULT_GAS_LIMIT: u64 = 10_000_000;

/// Default EIP-1559 base fee denominator used in tests.
pub const DEFAULT_DENOMINATOR: u32 = 50;

/// Default EIP-1559 elasticity multiplier used in tests.
pub const DEFAULT_ELASTICITY: u32 = 2;

/// Default JWT secret token for authenticating Engine API requests in tests.
pub const DEFAULT_JWT_TOKEN: &str =
    "688f5d737bad920bdfb2fc2f488d6b6209eebda1dae949a8de91398d932c517a";

/// One ETH expressed in wei.
pub const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// Returns a currently available local TCP port for tests.
pub fn get_available_port() -> u16 {
    let socket = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
    TcpListener::bind(socket)
        .expect("bind ephemeral local port")
        .local_addr()
        .expect("read local socket address")
        .port()
}

/// Clears telemetry environment variables that can affect CLI tests.
pub fn clear_otel_env_vars() {
    for key in [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_TRACES_EXPORTER",
        "OTEL_METRICS_EXPORTER",
    ] {
        // SAFETY: This helper is only used in single-threaded test setup before
        // the code under test starts reading process environment variables.
        unsafe {
            std::env::remove_var(key);
        }
    }
}

/// Default retry delay for local integration helpers.
pub const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(50);
