//! Builder Client CLI arguments with JWT handling.

use std::path::PathBuf;

use base_jwt::{JwtError, JwtSecret, resolve_jwt_secret};
use url::Url;

const DEFAULT_BUILDER_TIMEOUT: u64 = 30;

/// Rollup-boost builder client arguments.
#[derive(Clone, Debug, clap::Args)]
pub struct BuilderClientArgs {
    /// URL of the builder RPC API.
    #[arg(
        long,
        visible_alias = "builder",
        env = "BASE_NODE_BUILDER_RPC",
        default_value = "http://localhost:8552"
    )]
    pub l2_builder_rpc: Url,
    /// Hex encoded JWT secret to use for the authenticated builder RPC server.
    #[arg(long, visible_alias = "builder.auth", env = "BASE_NODE_BUILDER_AUTH")]
    pub builder_jwt_secret: Option<JwtSecret>,
    /// Path to a JWT secret to use for the authenticated builder RPC server.
    #[arg(long, visible_alias = "builder.jwt-path", env = "BASE_NODE_BUILDER_JWT_PATH")]
    pub builder_jwt_path: Option<PathBuf>,
    /// Timeout for http calls in milliseconds.
    #[arg(
        long,
        visible_alias = "builder.timeout",
        env = "BASE_NODE_BUILDER_TIMEOUT",
        default_value_t = DEFAULT_BUILDER_TIMEOUT
    )]
    pub builder_timeout: u64,
}

impl Default for BuilderClientArgs {
    fn default() -> Self {
        Self {
            l2_builder_rpc: Url::parse("http://localhost:8552").unwrap(),
            builder_jwt_secret: None,
            builder_jwt_path: None,
            builder_timeout: DEFAULT_BUILDER_TIMEOUT,
        }
    }
}

impl BuilderClientArgs {
    /// Returns the builder JWT secret.
    ///
    /// Resolution order:
    /// 1. Read from file path if `builder_jwt_path` is set
    /// 2. Use encoded secret if `builder_jwt_secret` is set
    /// 3. Fall back to default JWT file `builder_jwt.hex`
    pub fn jwt_secret(&self) -> Result<JwtSecret, JwtError> {
        resolve_jwt_secret(
            self.builder_jwt_path.as_deref(),
            self.builder_jwt_secret,
            "builder_jwt.hex",
        )
    }
}
