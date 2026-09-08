//! Revm is a Rust EVM implementation.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

// reexport dependencies
#[doc(inline)]
// Export items.
pub use base_evm_context::Context;
pub use base_evm_context::{Journal, JournalEntry};
#[doc(inline)]
pub use base_evm_handler::ExecuteCommitEvm;
#[doc(inline)]
pub use base_evm_handler::InspectCommitEvm;
pub use base_evm_handler::{
    ExecuteEvm, InspectEvm, InspectSystemCallEvm, Inspector, MainBuilder, MainContext, MainnetEvm,
    SystemCallCommitEvm, SystemCallEvm,
};
#[doc(inline)]
pub use base_state_api as database_interface;
pub use base_state_api::{Database, DatabaseCommit, DatabaseRef, NoopHook, OnStateHook};
#[doc(inline)]
pub use revm_bytecode as bytecode;
#[doc(inline)]
#[doc(inline)]
pub use revm_database as database;
#[doc(inline)]
pub use revm_interpreter as interpreter;
#[doc(inline)]
pub use revm_precompile as precompile;
pub use revm_precompile::install_crypto;
#[doc(inline)]
pub use revm_primitives as primitives;
#[doc(inline)]
pub use revm_state as state;
