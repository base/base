#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

#[macro_use]
mod macros;

mod aggregate_verifier;
pub use aggregate_verifier::{
    AggregateVerifierClient, AggregateVerifierContractClient, GameInfo, GameStatus,
    already_proven_selector, encode_challenge_calldata, encode_claim_credit_calldata,
    encode_nullify_calldata, encode_resolve_calldata, encode_verify_proposal_proof_calldata,
    invalid_parent_game_selector, invalid_signer_selector, l1_origin_too_old_selector,
};

mod delayed_weth;
pub use delayed_weth::{DelayedWETHClient, DelayedWETHContractClient};

mod anchor_state_registry;
pub use anchor_state_registry::{
    AnchorPreflight, AnchorRoot, AnchorSnapshot, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, encode_set_anchor_state_calldata,
};

mod dispute_game_factory;
pub use dispute_game_factory::{
    DisputeGameFactoryClient, DisputeGameFactoryContractClient, GameAtIndex, GameLookupError,
    GameLookupKey, encode_create_calldata, encode_extra_data, game_already_exists_selector,
    game_lookup_blocks, game_lookup_count, game_lookup_key,
};

mod tee_prover_registry;
pub use tee_prover_registry::{
    ITEEProverRegistry, TEEProverRegistryClient, TEEProverRegistryContractClient,
};

mod nitro_enclave_verifier;
pub use nitro_enclave_verifier::{
    INitroEnclaveVerifier, NitroEnclaveVerifierClient, NitroEnclaveVerifierContractClient,
    caller_not_owner_or_revoker_selector,
};

mod error;
pub use error::ContractError;
