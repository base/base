#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

extern crate alloc;

mod chain;
// Re-export base-alloy-hardforks types.
pub use base_alloy_hardforks::{OpHardfork, OpHardforks};
pub use chain::{
    BASE_MAINNET_HARDFORKS, BASE_SEPOLIA_HARDFORKS, DEV_HARDFORKS, OpChainHardforksExt,
};
