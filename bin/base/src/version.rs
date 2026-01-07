//! Version information for the Base binary.

/// Short version string.
pub const SHORT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Long version string with additional build info.
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\n", "Base Stack CLI");
