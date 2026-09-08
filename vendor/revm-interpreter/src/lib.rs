//! # revm-interpreter
//!
//! Interpreter is part of the project that executes EVM instructions.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

#[macro_use]
mod macros;

/// Gas calculation utilities and constants.
pub mod gas;
/// Context passed to instruction implementations.
pub mod instruction_context;
/// Instruction execution results and success/error types.
mod instruction_result;
/// EVM instruction implementations organized by category.
pub mod instructions;
/// Core interpreter implementation for EVM bytecode execution.
pub mod interpreter;
/// Types for interpreter actions like calls and contract creation.
pub mod interpreter_action;
/// Type traits and definitions for interpreter customization.
pub mod interpreter_types;

pub use base_evm_context as host;
// Reexport primary types.
pub use base_evm_context::CreateScheme;
pub use base_evm_context::{Host, InitialAndFloorGas, SStoreResult, SelfDestructResult, StateLoad};
pub use gas::{Gas, GasTracker};
pub use instruction_context::InstructionContext;
pub use instruction_result::*;
pub use instructions::{GasTable, Instruction, InstructionTable, gas_table, instruction_table};
pub use interpreter::{
    InputsImpl, Interpreter, InterpreterResult, STACK_LIMIT, SharedMemory, Stack, num_words,
};
pub use interpreter_action::{
    CallInput, CallInputs, CallOutcome, CallScheme, CallValue, CreateInputs, CreateOutcome,
    FrameInput, InterpreterAction,
};
pub use interpreter_types::InterpreterTypes;
pub use revm_bytecode as bytecode;
pub use revm_primitives as primitives;
pub use revm_state as state;
