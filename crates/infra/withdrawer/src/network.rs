use std::fmt;

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

/// Dispute-game contracts trusted by a fault-proof portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultProofContracts {
    /// Dispute game factory.
    pub dispute_game_factory: Address,
    /// Anchor state registry.
    pub anchor_state_registry: Address,
}

impl fmt::Display for FaultProofContracts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dispute game factory {}, anchor state registry {}",
            self.dispute_game_factory, self.anchor_state_registry
        )
    }
}

/// Proof system selection and optional custom contract expectations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofSystem {
    /// Dispute-game based fault proofs.
    FaultProofs {
        /// Custom contracts that must match the portal, absent for presets.
        expected: Option<FaultProofContracts>,
    },
    /// Legacy output-oracle proofs.
    OutputOracle {
        /// L2 output oracle.
        l2_output_oracle: Address,
    },
}

/// Concrete proof contracts resolved for one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedProofSystem {
    /// Contracts read from the configured fault-proof portal.
    FaultProofs(FaultProofContracts),
    /// Configured legacy output oracle, without portal discovery.
    OutputOracle {
        /// L2 output oracle.
        l2_output_oracle: Address,
    },
}

/// Configured L2 endpoint, portal, and proof system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// L2 RPC endpoint.
    pub l2_rpc_url: Url,
    /// Optimism portal.
    pub portal: Address,
    /// Proof system and custom contract expectations.
    pub proofs: ProofSystem,
    /// L1 chain ID the contracts live on, known for presets and unchecked for custom networks.
    pub l1_chain_id: Option<u64>,
}

impl NetworkPreset {
    /// Ethereum mainnet chain ID.
    pub const ETHEREUM_MAINNET_CHAIN_ID: u64 = 1;

    /// Ethereum Sepolia chain ID.
    pub const SEPOLIA_CHAIN_ID: u64 = 11_155_111;

    /// Resolves a preset's endpoint, portal, and expected L1 chain ID.
    pub fn resolve(self) -> Network {
        let (l1_chain_id, rpc, portal) = match self {
            Self::BaseMainnet => (
                Self::ETHEREUM_MAINNET_CHAIN_ID,
                "https://mainnet.base.org",
                address!("49048044D57e1C92A77f79988d21Fa8fAF74E97e"),
            ),
            Self::BaseSepolia => (
                Self::SEPOLIA_CHAIN_ID,
                "https://sepolia.base.org",
                address!("49f53e41452C74589E85cA1677426Ba426459e85"),
            ),
            Self::OpMainnet => (
                Self::ETHEREUM_MAINNET_CHAIN_ID,
                "https://mainnet.optimism.io",
                address!("bEb5Fc579115071764c7423A4f12eDde41f106Ed"),
            ),
            Self::OpSepolia => (
                Self::SEPOLIA_CHAIN_ID,
                "https://sepolia.optimism.io",
                address!("16Fc5058F25648194471939df75CF27A2fdC48BC"),
            ),
        };
        Network {
            l2_rpc_url: rpc.parse().expect("preset RPC URL is valid"),
            portal,
            proofs: ProofSystem::FaultProofs { expected: None },
            l1_chain_id: Some(l1_chain_id),
        }
    }
}
