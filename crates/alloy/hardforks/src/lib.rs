#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

mod hardfork;
pub use hardfork::OpHardfork;

mod hardforks;
pub use hardforks::OpHardforks;

mod chain;
pub use chain::OpChainHardforks;

mod mainnet;
pub use mainnet::{
    BASE_MAINNET_BEDROCK_BLOCK, BASE_MAINNET_CANYON_TIMESTAMP, BASE_MAINNET_ECOTONE_TIMESTAMP,
    BASE_MAINNET_FJORD_TIMESTAMP, BASE_MAINNET_GRANITE_TIMESTAMP,
    BASE_MAINNET_HOLOCENE_TIMESTAMP, BASE_MAINNET_ISTHMUS_TIMESTAMP,
    BASE_MAINNET_JOVIAN_TIMESTAMP, BASE_MAINNET_REGOLITH_TIMESTAMP,
};

mod sepolia;
pub use sepolia::{
    BASE_SEPOLIA_BEDROCK_BLOCK, BASE_SEPOLIA_CANYON_TIMESTAMP, BASE_SEPOLIA_ECOTONE_TIMESTAMP,
    BASE_SEPOLIA_FJORD_TIMESTAMP, BASE_SEPOLIA_GRANITE_TIMESTAMP,
    BASE_SEPOLIA_HOLOCENE_TIMESTAMP, BASE_SEPOLIA_ISTHMUS_TIMESTAMP,
    BASE_SEPOLIA_JOVIAN_TIMESTAMP, BASE_SEPOLIA_REGOLITH_TIMESTAMP,
};

pub use alloy_hardforks::{EthereumHardforks, ForkCondition};
