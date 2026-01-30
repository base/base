//! Engine configuration for the Base node.

use std::{sync::Arc, time::Duration};

use alloy_rpc_types_engine::JwtSecret;
use kona_genesis::RollupConfig;
use kona_node_service::NodeMode;
use url::Url;

/// Configuration for the Base node's engine.
///
/// This is a simplified configuration that only includes the fields needed
/// for the Base node, hiding the builder-related fields that are required
/// by kona's `EngineConfig` but not used by Base.
#[derive(Debug, Clone)]
pub struct BaseEngineConfig {
    /// The [`RollupConfig`].
    pub config: Arc<RollupConfig>,
    /// The engine RPC URL.
    pub l2_url: Url,
    /// The engine JWT secret.
    pub l2_jwt_secret: JwtSecret,
    /// The L2 timeout in milliseconds.
    pub l2_timeout_ms: u64,
    /// The L1 RPC URL.
    pub l1_url: Url,
    /// The mode of operation for the node.
    pub mode: NodeMode,
}

impl BaseEngineConfig {
    /// Creates a new [`BaseEngineConfig`].
    pub const fn new(
        config: Arc<RollupConfig>,
        l2_url: Url,
        l2_jwt_secret: JwtSecret,
        l2_timeout_ms: u64,
        l1_url: Url,
        mode: NodeMode,
    ) -> Self {
        Self { config, l2_url, l2_jwt_secret, l2_timeout_ms, l1_url, mode }
    }

    /// Returns the L2 timeout as a [`Duration`].
    pub const fn l2_timeout(&self) -> Duration {
        Duration::from_millis(self.l2_timeout_ms)
    }
}
