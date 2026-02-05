//! Container name constants for devnet infrastructure.

/// Name of the L1 Reth container.
pub const L1_RETH_NAME: &str = "l1-reth";
/// Name of the L1 Lighthouse beacon container.
pub const L1_BEACON_NAME: &str = "l1-beacon";
/// Name of the L1 Lighthouse validator container.
pub const L1_VALIDATOR_NAME: &str = "l1-validator";
/// Name of the L2 op-node container.
pub const L2_OP_NODE_NAME: &str = "l2-op-node";
/// Name of the L2 op-batcher container.
pub const L2_BATCHER_NAME: &str = "l2-batcher";
/// Name of the L2 client op-node container.
pub const L2_CLIENT_OP_NODE_NAME: &str = "l2-client-op-node";

/// HTTP port for L1 Beacon node.
pub const L1_BEACON_HTTP_PORT: u16 = 4052;
/// RPC port for L2 op-node (builder).
pub const L2_OP_NODE_RPC_PORT: u16 = 9545;
/// P2P port for L2 op-node (builder).
pub const L2_OP_NODE_P2P_PORT: u16 = 9222;
