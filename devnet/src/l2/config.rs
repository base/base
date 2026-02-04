//! Configuration for stable container naming and port binding for L2 components.

/// Configuration for stable container naming and port binding for L2 components.
#[derive(Debug, Clone, Default)]
pub struct L2ContainerConfig {
    /// If true, use stable container names instead of unique names
    pub use_stable_names: bool,
    /// Network name for all containers
    pub network_name: Option<String>,

    /// Host port for op-node RPC (maps to internal port 9545)
    pub op_node_rpc_port: Option<u16>,
    /// Host port for op-node P2P (maps to internal port 9222)
    pub op_node_p2p_port: Option<u16>,
    /// Host port for op-node follower RPC (maps to internal port 9545)
    pub op_node_follower_rpc_port: Option<u16>,
    /// Host port for batcher metrics
    pub batcher_metrics_port: Option<u16>,

    /// L2 Builder HTTP RPC port
    pub builder_http_port: Option<u16>,
    /// L2 Builder WebSocket port
    pub builder_ws_port: Option<u16>,
    /// L2 Builder Auth RPC port
    pub builder_auth_port: Option<u16>,
    /// L2 Builder P2P port
    pub builder_p2p_port: Option<u16>,
    /// L2 Builder Flashblocks port
    pub builder_flashblocks_port: Option<u16>,

    /// L2 Client HTTP RPC port
    pub client_http_port: Option<u16>,
    /// L2 Client WebSocket port
    pub client_ws_port: Option<u16>,
    /// L2 Client Auth RPC port
    pub client_auth_port: Option<u16>,
    /// L2 Client P2P port
    pub client_p2p_port: Option<u16>,
}
