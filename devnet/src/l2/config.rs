//! Configuration for stable container naming and port binding for L2 components.

/// Configuration for stable container naming and port binding for L2 components.
#[derive(Debug, Clone, Default)]
pub struct L2ContainerConfig {
    /// If true, use stable container names instead of unique names
    pub use_stable_names: bool,
    /// Network name for all containers
    pub network_name: Option<String>,

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

    /// Builder consensus RPC port
    pub builder_consensus_rpc_port: Option<u16>,
    /// Builder consensus P2P TCP port
    pub builder_consensus_p2p_tcp_port: Option<u16>,
    /// Builder consensus P2P UDP port
    pub builder_consensus_p2p_udp_port: Option<u16>,
    /// Client consensus RPC port
    pub client_consensus_rpc_port: Option<u16>,
    /// Client consensus P2P TCP port
    pub client_consensus_p2p_tcp_port: Option<u16>,
    /// Client consensus P2P UDP port
    pub client_consensus_p2p_udp_port: Option<u16>,
}
