//! Contract clients for the proposer.
//!
//! Shared bindings (dispute game factory, anchor state registry, aggregate verifier) are
//! re-exported from [`base_proof_common`]. The proposer-specific [`OutputProposer`] trait
//! and its implementations live in the [`output_proposer`] sub-module.

pub use base_proof_common::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorRoot,
    AnchorStateRegistryClient, AnchorStateRegistryContractClient, ContractError,
    DisputeGameFactoryClient, DisputeGameFactoryContractClient, GameAtIndex, GameInfo,
    encode_create_calldata, encode_extra_data, game_already_exists_selector,
};

mod output_proposer;
pub use output_proposer::*;
