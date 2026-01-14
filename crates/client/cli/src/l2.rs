//! L2 Client CLI arguments with JWT handling.

use std::path::PathBuf;

use alloy_rpc_types_engine::JwtSecret;
use url::Url;

const DEFAULT_L2_ENGINE_TIMEOUT: u64 = 30_000;
const DEFAULT_L2_TRUST_RPC: bool = true;

/// L2 client arguments.
#[derive(Clone, Debug, clap::Args)]
pub struct L2ClientArgs {
    /// URI of the engine API endpoint of an L2 execution client.
    #[arg(long, visible_alias = "l2", env = "KONA_NODE_L2_ENGINE_RPC")]
    pub l2_engine_rpc: Url,
    /// JWT secret for the auth-rpc endpoint of the execution client.
    /// This MUST be a valid path to a file containing the hex-encoded JWT secret.
    #[arg(long, visible_alias = "l2.jwt-secret", env = "KONA_NODE_L2_ENGINE_AUTH")]
    pub l2_engine_jwt_secret: Option<PathBuf>,
    /// Hex encoded JWT secret to use for the authenticated engine-API RPC server.
    /// This MUST be a valid hex-encoded JWT secret of 64 digits.
    #[arg(long, visible_alias = "l2.jwt-secret-encoded", env = "KONA_NODE_L2_ENGINE_AUTH_ENCODED")]
    pub l2_engine_jwt_encoded: Option<JwtSecret>,
    /// Timeout for http calls in milliseconds.
    #[arg(
        long,
        visible_alias = "l2.timeout",
        env = "KONA_NODE_L2_ENGINE_TIMEOUT",
        default_value_t = DEFAULT_L2_ENGINE_TIMEOUT
    )]
    pub l2_engine_timeout: u64,
    /// If false, block hash verification is performed for all retrieved blocks.
    #[arg(
        long,
        visible_alias = "l2.trust-rpc",
        env = "KONA_NODE_L2_TRUST_RPC",
        default_value_t = DEFAULT_L2_TRUST_RPC
    )]
    pub l2_trust_rpc: bool,
}

impl Default for L2ClientArgs {
    fn default() -> Self {
        Self {
            l2_engine_rpc: Url::parse("http://localhost:8551").unwrap(),
            l2_engine_jwt_secret: None,
            l2_engine_jwt_encoded: None,
            l2_engine_timeout: DEFAULT_L2_ENGINE_TIMEOUT,
            l2_trust_rpc: DEFAULT_L2_TRUST_RPC,
        }
    }
}
