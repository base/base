#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;
extern crate self as base_evm_context;

pub use base_state_api as database_interface;
pub use base_state_api::{DBErrorMarker, Database, ErasedError};
pub use either;
pub use revm_bytecode as bytecode;
pub use revm_primitives as primitives;
pub use revm_state as state;

mod block;
pub use block::*;

mod block_env;
pub use block_env::*;

mod cfg;
pub use cfg::*;

mod cfg_env;
pub use cfg_env::*;

mod context;
pub use context::*;

mod context_impl;
pub use context_impl::*;

mod evm;
pub use evm::*;

mod host;
pub use host::*;

mod journal;
pub use journal::*;

mod journaled_state;
pub use journaled_state::*;

mod local;
pub use local::*;

mod local_context;
pub use local_context::*;

mod result;
pub use result::*;

mod transaction;
pub use transaction::*;

mod tx;
pub use tx::*;
