//! Contains node versioning info.

use metrics::gauge;

/// Encapsulates versioning utilities for Base binaries.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Version;

impl Version {
    /// Exposes version information over Prometheus as `base_info{version="..."}`.
    pub fn register_metrics(version: &'static str) {
        let labels: [(&str, &str); 1] = [("version", version)];
        let gauge = gauge!("base_info", &labels);
        gauge.set(1);
    }
}

/// Registers version information as Prometheus metrics (`base_info{version="..."}`).
#[macro_export]
macro_rules! register_version_metrics {
    () => {
        $crate::Version::register_metrics(env!("CARGO_PKG_VERSION"))
    };
}
