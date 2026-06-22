//! In-process follow-mode consensus node for L2 system test stacks.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use alloy_provider::RootProvider;
use alloy_rpc_types_engine::JwtSecret;
use base_builder_core::test_utils::get_available_port;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_node::{EngineConfig, FollowNode, FollowNodeConfig, NodeMode, RemoteL2Client};
use base_consensus_rpc::RpcBuilder;
use eyre::{Result, WrapErr};
use tokio::task::JoinHandle;
use tracing::info;
use url::Url;

use super::in_process_consensus::wait_for_rpc;

/// Configuration for starting an in-process follow-mode consensus node.
#[derive(Debug)]
pub struct InProcessFollowConsensusConfig {
    /// Parsed rollup configuration.
    pub rollup_config: RollupConfig,
    /// JWT secret for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// L1 RPC endpoint URL.
    pub l1_rpc_url: Url,
    /// Local L2 execution RPC endpoint URL.
    pub local_l2_rpc_url: Url,
    /// Source L2 execution RPC endpoint URL to follow.
    pub source_l2_rpc_url: Url,
    /// Local L2 engine API URL.
    pub l2_engine_url: Url,
    /// Optional fixed RPC port.
    pub rpc_port: Option<u16>,
    /// Delay after each successful source payload insert.
    pub insert_delay: Duration,
}

/// A running in-process follow-mode consensus node.
pub struct InProcessFollowConsensus {
    rpc_addr: SocketAddr,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for InProcessFollowConsensus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessFollowConsensus").field("rpc_addr", &self.rpc_addr).finish()
    }
}

impl InProcessFollowConsensus {
    /// Starts an in-process follow-mode consensus node with the given configuration.
    pub async fn start(config: InProcessFollowConsensusConfig) -> Result<Self> {
        let rollup_config = Arc::new(config.rollup_config);
        let rpc_port = config.rpc_port.unwrap_or_else(get_available_port);
        let rpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);

        let engine_config = EngineConfig {
            config: Arc::clone(&rollup_config),
            l2_url: config.l2_engine_url,
            l2_jwt_secret: config.jwt_secret,
            l1_url: config.l1_rpc_url,
            mode: NodeMode::Validator,
        };
        let engine_client = Arc::new(
            engine_config
                .build_engine_client()
                .await
                .map_err(eyre::Report::from)
                .wrap_err("failed to build follow engine client")?,
        );
        let local_l2_provider = RootProvider::<Base>::new_http(config.local_l2_rpc_url);
        let l2_source = RemoteL2Client::new(config.source_l2_rpc_url);
        let rpc_builder = RpcBuilder {
            no_restart: true,
            socket: rpc_addr,
            enable_admin: false,
            admin_persistence: None,
            ws_enabled: false,
            dev_enabled: false,
            http_timeout: Duration::from_secs(60),
            max_concurrent_requests: NonZeroUsize::new(1024).expect("nonzero"),
        };

        let node = FollowNode::new(FollowNodeConfig {
            rollup_config,
            engine_client,
            local_l2_provider,
            l2_source,
            rpc_builder: Some(rpc_builder),
            proofs_enabled: false,
            proofs_max_blocks_ahead: 0,
            insert_delay: config.insert_delay,
        });

        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            info!(rpc_port = rpc_port, "starting in-process follow consensus node");
            if let Err(e) = node.start().await {
                let _ = startup_tx.send(eyre::eyre!("follow consensus node failed: {e}"));
            }
        });

        tokio::select! {
            result = startup_rx => {
                let error = result.wrap_err("startup channel closed")?;
                return Err(error).wrap_err("follow consensus node failed during startup");
            }
            result = wait_for_rpc(rpc_addr, "follow consensus RPC") => {
                result?;
            }
        }

        Ok(Self { rpc_addr, handle: Some(handle) })
    }

    /// Returns the RPC URL for this consensus node.
    pub fn rpc_url(&self) -> Url {
        Url::parse(&format!("http://{}:{}", self.rpc_addr.ip(), self.rpc_addr.port()))
            .expect("valid RPC URL")
    }

    /// Returns the RPC port.
    pub const fn rpc_port(&self) -> u16 {
        self.rpc_addr.port()
    }

    /// Stops the in-process follow consensus task and waits for Tokio to observe the abort.
    pub async fn shutdown(mut self) {
        if let Some(mut handle) = self.handle.take() {
            handle.abort();
            tokio::select! {
                result = &mut handle => {
                    let _ = result;
                }
                () = tokio::time::sleep(Duration::from_secs(5)) => {
                    drop(handle);
                }
            }
        }
    }
}

impl Drop for InProcessFollowConsensus {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
