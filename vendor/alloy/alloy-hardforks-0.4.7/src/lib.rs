#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

/// Re-exported EIP-2124 forkid types.
pub use alloy_eip2124::*;

mod forkcondition;
pub use forkcondition::*;

mod hardfork;
pub use hardfork::*;

/// Error types for the hardforks crate.
pub mod error;
pub use error::*;

pub mod ethereum;
pub use ethereum::{holesky, hoodi, mainnet, sepolia};
pub mod arbitrum;
pub use arbitrum::{mainnet as arbitrum_mainnet, sepolia as arbitrum_sepolia};

// Not public API.
#[doc(hidden)]
pub mod __private {
    pub use alloc::{format, string::String};
}
