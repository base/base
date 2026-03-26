#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod aggregate_verifier;
pub use aggregate_verifier::{
    AggregateVerifierClient, AggregateVerifierContractClient, GameInfo, encode_nullify_calldata,
};

mod anchor_state_registry;
pub use anchor_state_registry::{
    AnchorRoot, AnchorStateRegistryClient, AnchorStateRegistryContractClient,
};

mod dispute_game_factory;
pub use dispute_game_factory::{
    DisputeGameFactoryClient, DisputeGameFactoryContractClient, GameAtIndex,
    encode_create_calldata, encode_extra_data, game_already_exists_selector,
};

mod tee_prover_registry;
pub use tee_prover_registry::{
    ITEEProverRegistry, TEEProverRegistryClient, TEEProverRegistryContractClient,
};

mod error;
pub use error::ContractError;
