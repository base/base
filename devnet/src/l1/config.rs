//! Configuration for stable container naming and port binding.

/// Configuration for stable container naming and port binding.
/// Used when running containers with fixed names and ports (e.g., for CLI devnet).
#[derive(Debug, Clone, Default)]
pub struct L1ContainerConfig {
    /// If true, use stable container names (e.g., "l1-reth") instead of unique names
    pub use_stable_names: bool,
    /// If set, use this network instead of the default randomized network
    pub network_name: Option<String>,
    /// If set, bind to this specific host port for HTTP RPC
    pub http_port: Option<u16>,
    /// If set, bind to this specific host port for Engine API
    pub engine_port: Option<u16>,
    /// If set, bind to this specific host port for beacon HTTP API
    pub beacon_http_port: Option<u16>,
    /// If set, bind to this specific host port for beacon P2P
    pub beacon_p2p_port: Option<u16>,
}
