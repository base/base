#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod aggregate_verifier;
pub use aggregate_verifier::{
    AggregateVerifierClient, AggregateVerifierContractClient, GameInfo, encode_challenge_calldata,
    encode_claim_credit_calldata, encode_nullify_calldata, encode_resolve_calldata,
};

mod delayed_weth;
pub use delayed_weth::{DelayedWETHClient, DelayedWETHContractClient};

mod anchor_state_registry;
pub use anchor_state_registry::{
    AnchorPreflight, AnchorRoot, AnchorStateRegistryClient, AnchorStateRegistryContractClient,
    encode_set_anchor_state_calldata,
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

mod nitro_enclave_verifier;
pub use nitro_enclave_verifier::INitroEnclaveVerifier;

mod error;
pub use error::ContractError;
