//! Version information for the Base binary.
//!
//! [`VersionInfo`] metrics.
//!
//! Derived from [`reth-node-core`'s type][reth-version-info]
//!
//! [reth-version-info]: https://github.com/paradigmxyz/reth/blob/805fb1012cd1601c3b4fe9e8ca2d97c96f61355b/crates/node/metrics/src/version.rs#L6

use metrics::gauge;

/// Cargo package version.
pub const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build timestamp from vergen.
pub const VERGEN_BUILD_TIMESTAMP: &str = env!("VERGEN_BUILD_TIMESTAMP");

/// Cargo features from vergen.
pub const VERGEN_CARGO_FEATURES: &str = env!("VERGEN_CARGO_FEATURES");

/// Git SHA from vergen.
pub const VERGEN_GIT_SHA: &str = env!("VERGEN_GIT_SHA");

/// Cargo target triple from vergen.
pub const VERGEN_CARGO_TARGET_TRIPLE: &str = env!("VERGEN_CARGO_TARGET_TRIPLE");

/// Build profile name.
pub const BUILD_PROFILE_NAME: &str = env!("BASE_UNIFIED_BUILD_PROFILE");

/// Short version string.
pub const SHORT_VERSION: &str = env!("BASE_UNIFIED_SHORT_VERSION");

/// Long version string with additional build info.
pub const LONG_VERSION: &str = concat!(
    env!("BASE_UNIFIED_LONG_VERSION_0"),
    "\n",
    env!("BASE_UNIFIED_LONG_VERSION_1"),
    "\n",
    env!("BASE_UNIFIED_LONG_VERSION_2"),
    "\n",
    env!("BASE_UNIFIED_LONG_VERSION_3"),
    "\n",
    env!("BASE_UNIFIED_LONG_VERSION_4")
);

/// Contains version information for the application and allows for exposing the contained
/// information as a prometheus metric.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// The version of the application.
    pub version: &'static str,
    /// The build timestamp of the application.
    pub build_timestamp: &'static str,
    /// The cargo features enabled for the build.
    pub cargo_features: &'static str,
    /// The Git SHA of the build.
    pub git_sha: &'static str,
    /// The target triple for the build.
    pub target_triple: &'static str,
    /// The build profile (e.g., debug or release).
    pub build_profile: &'static str,
}

impl VersionInfo {
    /// Creates a new instance of [`VersionInfo`] from the constants defined in [`crate::version`]
    /// at compile time.
    pub const fn from_build() -> Self {
        Self {
            version: CARGO_PKG_VERSION,
            build_timestamp: VERGEN_BUILD_TIMESTAMP,
            cargo_features: VERGEN_CARGO_FEATURES,
            git_sha: VERGEN_GIT_SHA,
            target_triple: VERGEN_CARGO_TARGET_TRIPLE,
            build_profile: BUILD_PROFILE_NAME,
        }
    }

    /// Exposes version information over prometheus.
    pub fn register_version_metrics(&self) {
        // If no features are enabled, the string will be empty, and the metric will not be
        // reported. Report "none" if the string is empty.
        let features = if self.cargo_features.is_empty() { "none" } else { self.cargo_features };

        let labels: [(&str, &str); 6] = [
            ("version", self.version),
            ("build_timestamp", self.build_timestamp),
            ("cargo_features", features),
            ("git_sha", self.git_sha),
            ("target_triple", self.target_triple),
            ("build_profile", self.build_profile),
        ];

        let gauge = gauge!("base_node_info", &labels);
        gauge.set(1);
    }
}
