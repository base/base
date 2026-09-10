//! This exposes reth's version information over prometheus.
use metrics::gauge;

/// Contains version information for the application.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// The version of the application.
    pub version: &'static str,
}

impl VersionInfo {
    /// This exposes reth's version information over prometheus.
    pub fn register_version_metrics(&self) {
        let labels = [("version", self.version)];

        let gauge = gauge!("info", &labels);
        gauge.set(1);
    }
}
