//! Stable configuration for system test container names and ports.

/// Stable port assignments for system test components.
#[derive(Debug, Clone)]
pub struct SystemTestPorts {
    /// L1 HTTP RPC port
    pub l1_http: u16,
    /// L1 WebSocket port
    pub l1_ws: u16,
    /// L1 Auth RPC port
    pub l1_auth: u16,
    /// L1 P2P port
    pub l1_p2p: u16,
    /// L1 CL HTTP port
    pub l1_cl_http: u16,
    /// L1 CL P2P port
    pub l1_cl_p2p: u16,

    /// L2 Execution bootnode P2P port
    pub l2_el_bootnode_p2p: u16,
    /// L2 Consensus bootnode P2P port
    pub l2_cl_bootnode_p2p: u16,

    /// L2 Builder HTTP RPC port
    pub l2_builder_http: u16,
    /// L2 Builder WebSocket port
    pub l2_builder_ws: u16,
    /// L2 Builder Auth RPC port
    pub l2_builder_auth: u16,
    /// L2 Builder P2P port
    pub l2_builder_p2p: u16,
    /// L2 Builder Flashblocks port
    pub l2_builder_flashblocks: u16,
    /// L2 Builder Metrics port
    pub l2_builder_metrics: u16,
    /// L2 Builder CL RPC port
    pub l2_builder_cl_rpc: u16,
    /// L2 Builder CL P2P port
    pub l2_builder_cl_p2p: u16,
    /// L2 Builder CL Metrics port
    pub l2_builder_cl_metrics: u16,

    /// L2 Client HTTP RPC port
    pub l2_client_http: u16,
    /// L2 Client WebSocket port
    pub l2_client_ws: u16,
    /// L2 Client Auth RPC port
    pub l2_client_auth: u16,
    /// L2 Client P2P port
    pub l2_client_p2p: u16,
    /// L2 Client Metrics port
    pub l2_client_metrics: u16,
    /// L2 Client CL RPC port
    pub l2_client_cl_rpc: u16,
    /// L2 Client CL P2P port
    pub l2_client_cl_p2p: u16,
    /// L2 Client CL Metrics port
    pub l2_client_cl_metrics: u16,
}

impl SystemTestPorts {
    /// Returns the standard system test port assignments.
    pub const fn standard() -> Self {
        Self {
            l1_http: 4545,
            l1_ws: 4546,
            l1_auth: 4551,
            l1_p2p: 4303,
            l1_cl_http: 4052,
            l1_cl_p2p: 4900,

            l2_el_bootnode_p2p: 9303,
            l2_cl_bootnode_p2p: 9003,

            l2_builder_http: 7545,
            l2_builder_ws: 7546,
            l2_builder_auth: 7551,
            l2_builder_p2p: 7303,
            l2_builder_flashblocks: 7111,
            l2_builder_metrics: 7090,
            l2_builder_cl_rpc: 7549,
            l2_builder_cl_p2p: 7003,
            l2_builder_cl_metrics: 7300,

            l2_client_http: 8545,
            l2_client_ws: 8546,
            l2_client_auth: 8551,
            l2_client_p2p: 8303,
            l2_client_metrics: 8090,
            l2_client_cl_rpc: 8549,
            l2_client_cl_p2p: 8003,
            l2_client_cl_metrics: 8300,
        }
    }
}

/// Complete stable configuration for system tests.
#[derive(Debug, Clone)]
pub struct StableSystemTestConfig {
    /// Docker network name
    pub network_name: String,
    /// Port assignments
    pub ports: SystemTestPorts,
}

impl StableSystemTestConfig {
    /// Returns the standard system test configuration.
    pub fn standard() -> Self {
        Self { network_name: crate::network_name().to_string(), ports: SystemTestPorts::standard() }
    }
}
