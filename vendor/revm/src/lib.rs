//! Revm is a Rust EVM implementation.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

// reexport dependencies
#[doc(inline)]
pub use revm_bytecode as bytecode;
#[doc(inline)]
pub use revm_context as context;
// Export items.
pub use revm_context::{
    Context,
    journal::{Journal, JournalEntry},
};
#[doc(inline)]
pub use revm_context_interface as context_interface;
#[doc(inline)]
pub use revm_database as database;
#[doc(inline)]
pub use revm_database_interface as database_interface;
pub use revm_database_interface::{Database, DatabaseCommit, DatabaseRef, NoopHook, OnStateHook};
#[doc(inline)]
pub use revm_handler as handler;
pub use revm_handler::{
    ExecuteCommitEvm, ExecuteEvm, MainBuilder, MainContext, MainnetEvm, SystemCallCommitEvm,
    SystemCallEvm,
};
#[doc(inline)]
pub use revm_inspector as inspector;
pub use revm_inspector::{InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector};
#[doc(inline)]
pub use revm_interpreter as interpreter;
#[doc(inline)]
pub use revm_precompile as precompile;
pub use revm_precompile::install_crypto;
#[doc(inline)]
pub use revm_primitives as primitives;
#[doc(inline)]
pub use revm_state as state;
#[cfg(feature = "test-types")]
#[doc(inline)]
pub use revm_statetest_types_42_0_0 as statetest_types;
