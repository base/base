//! Logs directory utilities for CLI binaries.

use std::{ffi::OsString, path::PathBuf};

/// Encapsulates log directory utilities for Base binaries.
///
/// Provides a standardized way to determine the default log file directory
/// based on the system cache directory and package name.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LogsDir;

impl LogsDir {
    /// Returns the default log directory path for a package.
    ///
    /// The path is constructed as `{cache_dir}/{pkg_name}/logs` where `cache_dir`
    /// is the system's cache directory (e.g., `~/.cache` on Linux, `~/Library/Caches` on macOS).
    ///
    /// # Arguments
    ///
    /// * `pkg_name` - The package name to use in the path (typically from `CARGO_PKG_NAME`)
    ///
    /// # Panics
    ///
    /// Panics if the system cache directory cannot be determined.
    #[must_use]
    pub fn default_for_package(pkg_name: &str) -> OsString {
        Self::try_default_for_package(pkg_name)
            .expect("Unable to determine cache directory for log files")
    }

    /// Returns the default log directory path for a package, if available.
    ///
    /// The path is constructed as `{cache_dir}/{pkg_name}/logs` where `cache_dir`
    /// is the system's cache directory (e.g., `~/.cache` on Linux, `~/Library/Caches` on macOS).
    ///
    /// # Arguments
    ///
    /// * `pkg_name` - The package name to use in the path (typically from `CARGO_PKG_NAME`)
    ///
    /// # Returns
    ///
    /// Returns `Some(OsString)` with the log directory path, or `None` if the
    /// system cache directory cannot be determined.
    #[must_use]
    pub fn try_default_for_package(pkg_name: &str) -> Option<OsString> {
        dirs_next::cache_dir()
            .map(|root| root.join(format!("{pkg_name}/logs")))
            .map(PathBuf::into_os_string)
    }
}

/// Returns the default log directory path for the calling package.
///
/// This macro uses `CARGO_PKG_NAME` from the calling crate to construct
/// the log directory path as `{cache_dir}/{pkg_name}/logs`.
///
/// # Example
///
/// ```ignore
/// use base_cli_utils::logs_dir;
///
/// let logs_dir = logs_dir!();
/// // On macOS with package "base-builder": ~/Library/Caches/base-builder/logs
/// ```
///
/// # Panics
///
/// Panics if the system cache directory cannot be determined.
#[macro_export]
macro_rules! logs_dir {
    () => {
        $crate::LogsDir::default_for_package(env!("CARGO_PKG_NAME"))
    };
}

#[cfg(test)]
mod tests {
    use super::LogsDir;

    #[test]
    fn test_try_default_for_package_returns_some() {
        let result = LogsDir::try_default_for_package("test-pkg");
        assert!(result.is_some(), "Expected Some, got None - cache_dir unavailable");
    }

    #[test]
    fn test_default_for_package_contains_pkg_name() {
        let result = LogsDir::default_for_package("my-test-package");
        let path_str = result.to_string_lossy();
        assert!(
            path_str.contains("my-test-package"),
            "Expected path to contain 'my-test-package', got: {path_str}"
        );
    }

    #[test]
    fn test_default_for_package_ends_with_logs() {
        let result = LogsDir::default_for_package("test-pkg");
        let path_str = result.to_string_lossy();
        assert!(path_str.ends_with("logs"), "Expected path to end with 'logs', got: {path_str}");
    }

    #[test]
    fn test_default_for_package_format() {
        let result = LogsDir::default_for_package("base-builder");
        let path_str = result.to_string_lossy();
        assert!(
            path_str.contains("base-builder/logs"),
            "Expected path to contain 'base-builder/logs', got: {path_str}"
        );
    }
}
