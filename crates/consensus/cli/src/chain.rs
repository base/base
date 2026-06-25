//! Chain selection arguments for consensus clients.

use alloy_chains::Chain;
use clap::Args;

/// Non-global chain selection for reusable consensus CLI components.
#[derive(Args, Clone, Debug)]
pub struct ConsensusChainArgs {
    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(long = "chain", short = 'n', default_value = "8453", env = "BASE_NODE_NETWORK")]
    pub l2_chain_id: Chain,
}

impl Default for ConsensusChainArgs {
    fn default() -> Self {
        Self { l2_chain_id: Chain::from(8453_u64) }
    }
}

/// Global chain selection for the standalone `base-consensus` CLI.
#[derive(Args, Clone, Debug)]
pub struct GlobalConsensusChainArgs {
    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(
        long = "chain",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NODE_NETWORK"
    )]
    pub l2_chain_id: Chain,
}

impl From<GlobalConsensusChainArgs> for ConsensusChainArgs {
    fn from(args: GlobalConsensusChainArgs) -> Self {
        Self { l2_chain_id: args.l2_chain_id }
    }
}
