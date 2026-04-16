//! L1 Client CLI arguments.

use url::Url;

const DEFAULT_L1_TRUST_RPC: bool = true;

/// L1 client arguments.
#[derive(Clone, Debug, clap::Args)]
pub struct L1ClientArgs {
    /// URL of the L1 execution client RPC API.
    #[arg(long, visible_alias = "l1", env = "BASE_NODE_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,
    /// Whether to trust the L1 RPC.
    /// If false, block hash verification is performed for all retrieved blocks.
    #[arg(
        long,
        visible_alias = "l1.trust-rpc",
        env = "BASE_NODE_L1_TRUST_RPC",
        default_value_t = DEFAULT_L1_TRUST_RPC
    )]
    pub l1_trust_rpc: bool,
    /// URL of the L1 beacon API.
    #[arg(long, visible_alias = "l1.beacon", env = "BASE_NODE_L1_BEACON")]
    pub l1_beacon: Url,
    /// Duration in seconds of an L1 slot.
    ///
    /// This is an optional argument that can be used to use a fixed slot duration for l1 blocks
    /// and bypass the initial beacon spec fetch. This is useful for testing purposes when the
    /// l1-beacon spec endpoint is not available (with anvil for example).
    #[arg(
        long,
        visible_alias = "l1.slot-duration-override",
        env = "BASE_NODE_L1_SLOT_DURATION_OVERRIDE"
    )]
    pub l1_slot_duration_override: Option<u64>,
    /// Number of L1 blocks to keep distance from the L1 head for the verifier (derivation
    /// pipeline). Equivalent to op-node's `OP_NODE_VERIFIER_L1_CONFS`. Defaults to 0, meaning
    /// the verifier derives from the latest L1 head with no confirmation delay.
    #[arg(long = "l1.verifier-confs", default_value = "0", env = "BASE_NODE_VERIFIER_L1_CONFS")]
    pub l1_verifier_confs: u64,
}

impl Default for L1ClientArgs {
    fn default() -> Self {
        Self {
            l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
            l1_trust_rpc: DEFAULT_L1_TRUST_RPC,
            l1_beacon: Url::parse("http://localhost:5052").unwrap(),
            l1_slot_duration_override: None,
            l1_verifier_confs: 0,
        }
    }
}
