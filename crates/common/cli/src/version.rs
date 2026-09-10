//! Contains node versioning info.

use metrics::gauge;

/// Encapsulates versioning utilities for Base binaries.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Version;

impl Version {
    /// Exposes version information over Prometheus as
    /// `base_info{version="...",binary="..."}`.
    pub fn register_metrics(version: &'static str, binary: &'static str) {
        let labels: [(&str, &str); 2] = [("version", version), ("binary", binary)];
        let gauge = gauge!("base_info", &labels);
        gauge.set(1);
    }
}

/// Registers version information as Prometheus metrics
/// (`base_info{version="...",binary="..."}`).
#[macro_export]
macro_rules! register_version_metrics {
    () => {
        $crate::Version::register_metrics(
            env!("CARGO_PKG_VERSION"),
            option_env!("CARGO_BIN_NAME").unwrap_or(env!("CARGO_PKG_NAME")),
        )
    };
}
