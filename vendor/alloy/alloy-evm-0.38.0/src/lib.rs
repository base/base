#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod block;
pub mod evm;
pub use evm::{Database, Evm, EvmFactory};
pub mod eth;
pub use eth::{EthEvm, EthEvmFactory};
pub mod env;
pub use env::{EvmEnv, EvmLimitParams, TransactionEnvMut};
pub mod error;
pub use error::*;
pub mod tx;
pub use tx::*;
pub mod traits;
pub use traits::*;
#[cfg(feature = "call-util")]
pub mod call;
#[cfg(feature = "overrides")]
pub mod overrides;
pub mod precompiles;
pub use precompiles::MovePrecompileError;
#[cfg(feature = "rpc")]
pub mod rpc;
pub mod tracing;

mod either;

// re-export revm
pub use revm;

pub use eth::spec_id::{spec, spec_by_timestamp_and_block_number};
