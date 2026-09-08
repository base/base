#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;
extern crate self as base_evm_handler;

pub use base_state_api as database_interface;
pub use revm_bytecode as bytecode;
pub use revm_interpreter as interpreter;
pub use revm_precompile as precompile;
pub use revm_primitives as primitives;
pub use revm_state as state;

mod api;
pub use api::*;

mod count_inspector;
pub use count_inspector::*;

#[cfg(feature = "tracer")]
mod eip3155;
#[cfg(feature = "tracer")]
pub use eip3155::*;

mod either;

mod evm;
pub use evm::{ContextDbError, EvmTr, FrameInitResult, FrameTr};

mod execution;
pub use execution::*;

mod frame;
pub use frame::*;

mod frame_data;
pub use frame_data::*;

mod gas;
pub use gas::*;

mod handler;
pub use handler::*;

mod inspect;
pub use inspect::*;

mod inspector;
pub use inspector::*;

mod inspector_handler;
pub use inspector_handler::*;

mod instructions;
pub use instructions::*;

mod item_or_result;
pub use item_or_result::*;

mod mainnet_builder;
pub use mainnet_builder::*;

mod mainnet_handler;
pub use mainnet_handler::*;

mod mainnet_inspect;

mod noop;
pub use noop::*;

mod post_execution;
pub use post_execution::*;

mod pre_execution;
pub use pre_execution::*;

mod precompile_provider;
pub use precompile_provider::*;

mod system_call;
pub use system_call::*;

mod test_inspector;
pub use test_inspector::*;

mod traits;
pub use traits::*;

mod validation;
pub use validation::*;
