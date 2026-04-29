#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod base;
pub use base::BASE_MAINNET;

mod base_zeronet;
pub use base_zeronet::BASE_ZERONET;

mod base_sepolia;
pub use base_sepolia::BASE_SEPOLIA;

mod basefee;
pub use basefee::*;

mod builder;
pub use builder::BaseChainSpecBuilder;

mod dev;
pub use dev::BASE_DEV;

mod hardforks;
pub use hardforks::{
    BASE_MAINNET_UPGRADES, BASE_SEPOLIA_UPGRADES, BASE_ZERONET_UPGRADES, ChainUpgradesExt,
    DEV_UPGRADES,
};

mod spec;
pub use spec::{BaseChainSpec, GenesisInfo, SUPPORTED_CHAINS};
