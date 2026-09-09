#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(not(feature = "std"))]
extern crate alloc as std;
extern crate self as base_evm_handler;

pub use base_evm_context::{Context, Journal, JournalEntry};
pub use base_state as database;
pub use base_state::{DatabaseCommit, DatabaseRef, NoopHook, OnStateHook};
pub use revm_bytecode as bytecode;
pub use revm_interpreter as interpreter;
pub use revm_precompile as precompile;
pub use revm_precompile::install_crypto;
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

mod machine;
pub use machine::EvmMachine;

mod evm;
pub use evm::{ContextDbError, EvmTr, FrameInitResult};

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

mod block;
pub use block::*;

mod evm_api;
pub use evm_api::*;

mod eth;
pub use eth::*;

pub use base_evm_context::{BlockEnvironment, EvmEnv, EvmLimitParams, TransactionEnvMut};

mod error;
pub use error::*;

mod tx;
pub use tx::*;

mod evm_internals;
pub use evm_internals::*;

#[cfg(feature = "call-util")]
mod call;
#[cfg(feature = "call-util")]
pub use call::*;

#[cfg(feature = "overrides")]
mod overrides;
#[cfg(feature = "overrides")]
pub use overrides::*;

mod precompiles;
pub use precompiles::*;

#[cfg(feature = "rpc")]
mod rpc;
#[cfg(feature = "rpc")]
pub use rpc::*;

mod tracing;
pub use tracing::*;

mod either_evm;

mod base_transactions;
