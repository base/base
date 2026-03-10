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

mod base_devnet_0_sepolia_dev_0;
pub use base_devnet_0_sepolia_dev_0::BASE_DEVNET_0_SEPOLIA_DEV_0;

mod base_sepolia;
pub use base_sepolia::BASE_SEPOLIA;

mod basefee;
pub use basefee::*;

mod builder;
pub use builder::OpChainSpecBuilder;

mod constants;
pub use constants::*;

mod dev;
pub use dev::OP_DEV;

mod spec;
pub use spec::{OpChainSpec, OpGenesisInfo, SUPPORTED_CHAINS};
