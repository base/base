//! EVM execution context interface.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub mod block;
pub mod cfg;
pub mod context;
pub mod host;
pub mod journaled_state;
pub mod local;
pub mod result;
pub mod transaction;

pub use base_state_api as database_interface;
pub use base_state_api::{DBErrorMarker, Database, ErasedError};
pub use block::Block;
pub use cfg::{Cfg, CreateScheme, TransactTo};
pub use context::{ContextError, ContextSetters, ContextTr};
pub use either;
pub use host::{DummyHost, Host};
pub use journaled_state::JournalTr;
pub use local::{FrameStack, FrameToken, LocalContextTr, OutFrame};
pub use revm_primitives as primitives;
pub use revm_state as state;
pub use transaction::{Transaction, TransactionType};
