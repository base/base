//! Base EVM context, journal, and bytecode interpreter.

pub use either;

pub(crate) use crate::{Database, DatabaseError as ErasedError};

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
pub use environment::{EvmEnv, EvmLimitParams, TransactionEnvMut};

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
pub use interpreter::{InputsImpl, Interpreter, InterpreterResult, SharedMemory, Stack};

/// Interpreter call, creation, and return actions.
pub mod interpreter_action;
pub use interpreter_action::{
    CallInput, CallInputs, CallOutcome, CallScheme, CallValue, CreateInputs, CreateOutcome,
    FrameInput, InterpreterAction,
};

#[cfg(any(test, feature = "test-utils"))]
mod reference_context;
#[cfg(any(test, feature = "test-utils"))]
pub use environment::EthEvmContext;
#[cfg(any(test, feature = "test-utils"))]
pub use reference_context::ReferenceContext;
