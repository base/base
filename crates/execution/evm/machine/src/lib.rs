#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;
extern crate self as base_execution_evm_machine;

pub use base_execution_state_memory as database;
pub use base_execution_state_memory as state;
pub use base_execution_state_memory::{DBErrorMarker, Database, ErasedError};
pub use either;
pub use revm_bytecode as bytecode;
pub use revm_primitives as primitives;

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

mod environment;
pub use environment::{BlockEnvironment, EthEvmContext, EvmEnv, EvmLimitParams, TransactionEnvMut};

#[macro_use]
mod macros;

mod instruction_context;
pub use instruction_context::InstructionContext;

mod instruction_result;
pub use instruction_result::*;

/// Gas accounting and opcode costs.
pub mod gas;
pub use gas::{Gas, GasTracker};

/// EVM instruction implementations and dispatch tables.
pub mod instructions;
pub use instructions::{GasTable, Instruction, InstructionTable, gas_table, instruction_table};

/// Bytecode interpreter and its memory and stack.
pub mod interpreter;
pub use interpreter::{
    InputsImpl, Interpreter, InterpreterResult, STACK_LIMIT, SharedMemory, Stack,
};

/// Interpreter call, creation, and return actions.
pub mod interpreter_action;
pub use interpreter_action::{
    CallInput, CallInputs, CallOutcome, CallScheme, CallValue, CreateInputs, CreateOutcome,
    FrameInput, InterpreterAction,
};
