//! L2 Client CLI arguments with JWT handling.

use std::path::PathBuf;

use alloy_rpc_types_engine::JwtSecret;
use base_jwt::{JwtError, JwtSecretReader, JwtValidator};
use url::Url;

const DEFAULT_L2_ENGINE_TIMEOUT: u64 = 30_000;
const DEFAULT_L2_TRUST_RPC: bool = true;

/// L2 client arguments.
#[derive(Clone, Debug)]
pub struct L2ClientArgs {
    /// URI of the engine API endpoint of an L2 execution client.
    pub l2_engine_rpc: Url,
    /// JWT secret for the auth-rpc endpoint of the execution client.
    /// This MUST be a valid path to a file containing the hex-encoded JWT secret.
    pub l2_engine_jwt_secret: Option<PathBuf>,
    /// Hex encoded JWT secret to use for the authenticated engine-API RPC server.
    /// This MUST be a valid hex-encoded JWT secret of 64 digits.
    pub l2_engine_jwt_encoded: Option<JwtSecret>,
    /// Timeout for http calls in milliseconds.
    pub l2_engine_timeout: u64,
    /// If false, block hash verification is performed for all retrieved blocks.
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

impl L2ClientArgs {
    /// Returns the L2 JWT secret for the engine API.
    ///
    /// Resolution order:
    /// 1. Read from file path if `l2_engine_jwt_secret` is set
    /// 2. Use encoded secret if `l2_engine_jwt_encoded` is set
    /// 3. Fall back to default JWT file `l2_jwt.hex`
    pub fn jwt_secret(&self) -> Result<JwtSecret, JwtError> {
        JwtSecretReader::resolve_jwt_secret(
            self.l2_engine_jwt_secret.as_deref(),
            self.l2_engine_jwt_encoded,
            "l2_jwt.hex",
        )
    }

    /// Validate the jwt secret if specified by exchanging capabilities with the engine.
    /// Since the engine client will fail if the jwt token is invalid, this allows to ensure
    /// that the jwt token passed as a cli arg is correct.
    pub async fn validate_jwt(&self) -> eyre::Result<JwtSecret> {
        let jwt_secret = self.jwt_secret().map_err(|e| eyre::eyre!(e))?;
        let validator = JwtValidator::new(jwt_secret);
        validator.validate_with_engine(self.l2_engine_rpc.clone()).await.map_err(|e| eyre::eyre!(e))
    }
}
