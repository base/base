//! Engine API endpoint configuration.

use std::{net::SocketAddr, path::Path};

use alloy_rpc_types_engine::JwtSecret;
use eyre::Result;
use reth_node_core::node_config::NodeConfig;
use reth_optimism_chainspec::OpChainSpec;
use tracing::info;
use url::Url;

/// Engine API endpoint with URL and JWT authentication.
#[derive(Debug, Clone)]
pub struct EngineEndpoint {
    /// Engine API URL.
    pub url: Url,
    /// JWT secret for authentication.
    pub jwt_secret: JwtSecret,
}

impl EngineEndpoint {
    /// Extracts endpoint configuration from [`NodeConfig`].
    pub fn from_reth_config(config: &NodeConfig<OpChainSpec>) -> Result<Self> {
        let addr = SocketAddr::new(config.rpc.auth_addr, config.rpc.auth_port);
        let url = Url::parse(&format!("http://{}", addr))
            .map_err(|e| eyre::eyre!("Failed to parse engine URL: {e}"))?;

        let jwt_secret = Self::resolve_jwt_secret(config)?;

        Ok(Self { url, jwt_secret })
    }

    /// Resolves JWT secret from config (inline, file path, default path, or random).
    fn resolve_jwt_secret(config: &NodeConfig<OpChainSpec>) -> Result<JwtSecret> {
        if let Some(secret) = &config.rpc.rpc_jwtsecret {
            return Ok(*secret);
        }

        if let Some(path) = &config.rpc.auth_jwtsecret {
            return Self::read_jwt_from_path(path);
        }

        let default_path = config.datadir().jwt();
        if default_path.exists() {
            return Self::read_jwt_from_path(&default_path);
        }

        info!(target: "unified", "No JWT secret found, generating random one");
        Ok(JwtSecret::random())
    }

    /// Reads and parses a JWT secret from a file.
    fn read_jwt_from_path(path: &Path) -> Result<JwtSecret> {
        let hex = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("Failed to read JWT secret from {:?}: {e}", path))?;

        JwtSecret::from_hex(hex.trim()).map_err(|e| eyre::eyre!("Failed to parse JWT secret: {e}"))
    }
}
