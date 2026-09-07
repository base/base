//! EVM execution handling.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

// Mainnet related handlers.

/// EVM execution API traits and implementations.
pub mod api;
/// Core EVM traits for execution and frame management.
pub mod evm;
/// EVM execution logic and utilities.
pub mod execution;
mod frame;
mod frame_data;
/// Handler implementation for orchestrating EVM execution.
pub mod handler;
/// EVM instruction set implementations and tables.
pub mod instructions;
mod item_or_result;
mod mainnet_builder;
mod mainnet_handler;
/// Post-execution operations including gas refunds and state finalization.
pub mod post_execution;
pub mod pre_execution;
mod precompile_provider;
/// System call implementations for special EVM operations.
pub mod system_call;
/// Transaction and environment validation utilities.
pub mod validation;

// Public exports
pub use api::{ExecuteCommitEvm, ExecuteEvm};
pub use evm::{EvmTr, FrameTr};
pub use frame::{ContextTrDbError, EthFrame, handle_reservoir_remaining_gas, return_create};
pub use frame_data::{CallFrame, CreateFrame, FrameData, FrameResult};
pub use handler::{EvmTrError, Handler};
pub use item_or_result::{FrameInitOrResult, ItemOrResult};
pub use mainnet_builder::{MainBuilder, MainContext, MainnetContext, MainnetEvm};
pub use mainnet_handler::MainnetHandler;
pub use pre_execution::PreExecutionOutput;
pub use precompile_provider::{
    EthPrecompiles, PrecompileProvider, precompile_output_to_interpreter_result,
};
pub use revm_bytecode as bytecode;
pub use revm_context as context;
pub use revm_context_interface as context_interface;
pub use revm_database_interface as database_interface;
pub use revm_interpreter as interpreter;
pub use revm_precompile as precompile;
pub use revm_primitives as primitives;
pub use revm_state as state;
pub use system_call::{SYSTEM_ADDRESS, SystemCallCommitEvm, SystemCallEvm, SystemCallTx};
