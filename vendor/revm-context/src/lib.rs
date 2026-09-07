//! EVM execution context.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub use revm_bytecode as bytecode;
pub use revm_context_interface as context_interface;
pub use revm_context_interface::*;
pub use revm_database_interface as database_interface;
pub use revm_primitives as primitives;
pub use revm_state as state;

pub mod block;
pub mod cfg;
pub mod context;
pub mod evm;
pub mod journal;
pub mod local;
pub mod tx;

pub use block::BlockEnv;
pub use cfg::{Cfg, CfgEnv};
pub use context::*;
pub use evm::Evm;
pub use journal::*;
pub use local::LocalContext;
pub use tx::TxEnv;
