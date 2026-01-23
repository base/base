//! Build version information and metrics.

use metrics::gauge;

/// Cargo package version.
pub const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Build timestamp.
pub const VERGEN_BUILD_TIMESTAMP: &str = env!("VERGEN_BUILD_TIMESTAMP");
/// Enabled cargo features.
pub const VERGEN_CARGO_FEATURES: &str = env!("VERGEN_CARGO_FEATURES");
/// Git commit SHA.
pub const VERGEN_GIT_SHA: &str = env!("VERGEN_GIT_SHA");
/// Target triple.
pub const VERGEN_CARGO_TARGET_TRIPLE: &str = env!("VERGEN_CARGO_TARGET_TRIPLE");
/// Build profile (debug/release).
pub const BUILD_PROFILE_NAME: &str = env!("BASE_UNIFIED_BUILD_PROFILE");
/// Short version string.
pub const SHORT_VERSION: &str = env!("BASE_UNIFIED_SHORT_VERSION");
/// Long version string with build info.
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

/// Build version info for prometheus metrics.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// Application version.
    pub version: &'static str,
    /// Build timestamp.
    pub build_timestamp: &'static str,
    /// Enabled cargo features.
    pub cargo_features: &'static str,
    /// Git commit SHA.
    pub git_sha: &'static str,
    /// Target triple.
    pub target_triple: &'static str,
    /// Build profile.
    pub build_profile: &'static str,
}

impl VersionInfo {
    /// Creates [`VersionInfo`] from compile-time constants.
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

    /// Registers version info as prometheus metrics.
    pub fn register_version_metrics(&self) {
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
