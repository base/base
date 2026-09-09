//! RPC health response.

use alloc::string::String;

/// A healthcheck response for the RPC server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HealthzResponse {
    /// The application version.
    pub version: String,
}
