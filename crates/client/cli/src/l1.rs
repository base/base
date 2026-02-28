//! L1 Client CLI arguments.

use url::Url;

const DEFAULT_L1_TRUST_RPC: bool = true;

/// L1 client arguments.
#[derive(Clone, Debug)]
pub struct L1ClientArgs {
    /// URL of the L1 execution client RPC API.
    pub l1_eth_rpc: Url,
    /// Whether to trust the L1 RPC.
    /// If false, block hash verification is performed for all retrieved blocks.
    pub l1_trust_rpc: bool,
    /// URL of the L1 beacon API.
    pub l1_beacon: Url,
    /// Duration in seconds of an L1 slot.
    ///
    /// This is an optional argument that can be used to use a fixed slot duration for l1 blocks
    /// and bypass the initial beacon spec fetch. This is useful for testing purposes when the
    /// l1-beacon spec endpoint is not available (with anvil for example).
    pub l1_slot_duration_override: Option<u64>,
}

impl Default for L1ClientArgs {
    fn default() -> Self {
        Self {
            l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
            l1_trust_rpc: DEFAULT_L1_TRUST_RPC,
            l1_beacon: Url::parse("http://localhost:5052").unwrap(),
            l1_slot_duration_override: None,
        }
    }
}
