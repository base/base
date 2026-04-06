#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

extern crate alloc;

mod chain;
pub use chain::{
    BASE_DEVNET_0_SEPOLIA_DEV_0_UPGRADES, BASE_MAINNET_UPGRADES, BASE_SEPOLIA_UPGRADES,
    BASE_ZERONET_UPGRADES, BaseChainUpgradesExt, DEV_UPGRADES,
};
