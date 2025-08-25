use alloy_primitives::{Address, U256, utils::parse_ether};

use crate::tx_signer::Signer;

/// Parameters for Flashtestations configuration
/// The names in the struct are prefixed with `flashtestations`
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct FlashtestationsArgs {
    /// When set to true, the builder will initiate the flashtestations
    /// workflow within the bootstrapping and block building process.
    #[arg(
        long = "flashtestations.enabled",
        default_value = "false",
        env = "ENABLE_FLASHTESTATIONS"
    )]
    pub flashtestations_enabled: bool,

    /// Whether to use the debug HTTP service for quotes
    #[arg(
        long = "flashtestations.debug",
        default_value = "false",
        env = "FLASHTESTATIONS_DEBUG"
    )]
    pub debug: bool,

    // Debug url for attestations
    #[arg(long = "flashtestations.debug-url", env = "FLASHTESTATIONS_DEBUG_URL")]
    pub debug_url: Option<String>,

    /// The rpc url to post the onchain attestation requests to
    #[arg(
        long = "flashtestations.rpc-url",
        env = "FLASHTESTATIONS_RPC_URL",
        default_value = "http://localhost:8545"
    )]
    pub rpc_url: String,

    /// Funding key for the TEE key
    #[arg(
        long = "flashtestations.funding-key",
        env = "FLASHTESTATIONS_FUNDING_KEY",
        required_if_eq("flashtestations_enabled", "true")
    )]
    pub funding_key: Option<Signer>,

    /// Funding amount for the generated signer
    #[arg(
        long = "flashtestations.funding-amount",
        env = "FLASHTESTATIONS_FUNDING_AMOUNT",
        default_value = "1",
        value_parser = parse_ether
    )]
    pub funding_amount: U256,

    /// Enable end of block TEE proof
    #[arg(
        long = "flashtestations.enable-block-proofs",
        env = "FLASHTESTATIONS_ENABLE_BLOCK_PROOFS",
        default_value = "false"
    )]
    pub enable_block_proofs: bool,

    /// The address of the flashtestations registry contract
    #[arg(
        long = "flashtestations.registry-address",
        env = "FLASHTESTATIONS_REGISTRY_ADDRESS",
        required_if_eq("flashtestations_enabled", "true")
    )]
    pub registry_address: Option<Address>,

    /// The address of the builder policy contract
    #[arg(
        long = "flashtestations.builder-policy-address",
        env = "FLASHTESTATIONS_BUILDER_POLICY_ADDRESS",
        required_if_eq("flashtestations_enabled", "true")
    )]
    pub builder_policy_address: Option<Address>,

    /// The version of the block builder verification proof
    #[arg(
        long = "flashtestations.builder-proof-version",
        env = "FLASHTESTATIONS_BUILDER_PROOF_VERSION",
        default_value = "1"
    )]
    pub builder_proof_version: u8,
}
