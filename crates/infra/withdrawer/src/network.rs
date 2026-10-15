use alloy_primitives::{Address, address};
use clap::ValueEnum;
use url::Url;

/// Built-in OP Stack network selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum NetworkPreset {
    /// Base mainnet.
    #[default]
    BaseMainnet,
    /// Base Sepolia.
    BaseSepolia,
    /// OP mainnet.
    OpMainnet,
    /// OP Sepolia.
    OpSepolia,
}

/// Contracts used to prove a withdrawal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofSystem {
    /// Dispute-game based fault proofs.
    FaultProofs {
        /// Dispute game factory.
        dispute_game_factory: Address,
        /// Anchor state registry.
        anchor_state_registry: Address,
    },
    /// Legacy output-oracle proofs.
    OutputOracle {
        /// L2 output oracle.
        l2_output_oracle: Address,
    },
}

/// Resolved L2 endpoint and L1 withdrawal contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// L2 RPC endpoint.
    pub l2_rpc_url: Url,
    /// Optimism portal.
    pub portal: Address,
    /// Proof contracts.
    pub proofs: ProofSystem,
}

impl NetworkPreset {
    /// Resolves a preset into its public endpoint and contracts.
    pub fn resolve(self) -> Network {
        let (rpc, portal, dispute_game_factory, anchor_state_registry) = match self {
            Self::BaseMainnet => (
                "https://mainnet.base.org",
                address!("49048044D57e1C92A77f79988d21Fa8fAF74E97e"),
                address!("43edB88C4B80fDD2AdFF2412A7BebF9dF42cB40e"),
                address!("909f6cf47ed12f010A796527f562bFc26C7F4E72"),
            ),
            Self::BaseSepolia => (
                "https://sepolia.base.org",
                address!("49f53e41452C74589E85cA1677426Ba426459e85"),
                address!("d6E6dBf4F7EA0ac412fD8b65ED297e64BB7a06E1"),
                address!("2fF5cC82dBf333Ea30D8ee462178ab1707315355"),
            ),
            Self::OpMainnet => (
                "https://mainnet.optimism.io",
                address!("bEb5Fc579115071764c7423A4f12eDde41f106Ed"),
                address!("e5965Ab5962eDc7477C8520243A95517CD252fA9"),
                address!("23B2C62946350F4246f9f9D027e071f0264FD113"),
            ),
            Self::OpSepolia => (
                "https://sepolia.optimism.io",
                address!("16Fc5058F25648194471939df75CF27A2fdC48BC"),
                address!("05F9613aDB30026FFd634f38e5C4dFd30a197Fa1"),
                address!("a1Cec548926eb5d69aa3B7B57d371EdBdD03e64b"),
            ),
        };
        Network {
            l2_rpc_url: rpc.parse().expect("preset RPC URL is valid"),
            portal,
            proofs: ProofSystem::FaultProofs { dispute_game_factory, anchor_state_registry },
        }
    }
}
