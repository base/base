#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod contracts;
pub use contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorRoot,
    AnchorStateRegistryClient, AnchorStateRegistryContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameAtIndex, GameInfo, encode_create_calldata,
    encode_extra_data, game_already_exists_selector,
};

mod error;
pub use error::ContractError;
