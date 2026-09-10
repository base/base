#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(not(feature = "std"))]
extern crate alloc as std;
extern crate self as base_execution_evm_runtime;

pub use base_common_chain_config::BaseUpgrade;

mod spec;
pub use spec::BaseSpecId;

mod result;
pub use result::BaseHaltReason;

mod l1block;
pub use l1block::L1BlockInfo;

mod base_time;
pub use base_time::{BaseTime, BaseTimeTransitionError};

mod transaction;
pub use transaction::{
    BaseTransaction, BaseTransactionBuilder, BaseTransactionError, BuildError,
    DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts, EIP8130_TRANSACTION_TYPE,
    Eip8130ExecutionMode, Eip8130TransactionParts,
};

mod handler;
pub use handler::BaseHandler;

mod precompiles;

mod beryl_metrics;
pub use beryl_metrics::BerylPrecompileMetricsObserver;

mod api;
pub use api::{BaseContext, BaseError, Builder, DefaultBase};

mod evm;
pub use evm::BaseEvm;

#[cfg(feature = "std")]
mod eip8130;
#[cfg(feature = "std")]
pub use eip8130::{Eip8130Executor, Eip8130Outcome};

mod eip8130_phase_statuses;
pub use eip8130_phase_statuses::Eip8130PhaseStatuses;

mod factory;
pub use factory::BaseEvmFactory;

mod error;
pub use error::BaseBlockExecutionError;

mod canyon;
pub use canyon::ensure_create2_deployer;

mod zenith;
pub use zenith::ensure_eip8130_system_accounts;

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseBlockExecutorFactory, BaseTxResult,
};

mod execution_api;
pub use execution_api::*;

mod count_inspector;
pub use count_inspector::*;

#[path = "either.rs"]
mod either_impl;

mod machine;
pub use machine::EvmMachine;

mod machine_traits;
pub use machine_traits::{ContextDbError, EvmTr, FrameInitResult};

mod execution;
pub use execution::*;

mod frame;
pub use frame::*;

mod frame_data;
pub use frame_data::*;

#[path = "gas.rs"]
mod execution_gas;
pub use execution_gas::*;

mod execution_handler;
pub use execution_handler::*;

mod inspect;
pub use inspect::*;

mod inspector;
pub use inspector::*;

mod inspector_handler;
pub use inspector_handler::*;

#[path = "instructions.rs"]
mod execution_instructions;
pub use execution_instructions::*;

mod item_or_result;
pub use item_or_result::*;

#[cfg(any(test, feature = "test-utils"))]
mod mainnet_builder;
#[cfg(any(test, feature = "test-utils"))]
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

#[cfg(any(test, feature = "test-utils"))]
mod eth;
#[cfg(any(test, feature = "test-utils"))]
pub use eth::ReferenceEvmEnv;
#[cfg(any(test, feature = "test-utils"))]
pub use eth::*;

mod execution_error;
pub use execution_error::*;

mod tx;
pub use tx::*;

mod tracing;
pub use tracing::*;

mod base_transactions;

mod eth_tx_result;
pub use eth_tx_result::*;

mod core_primitives;
pub use core_primitives::*;

mod core_memory;
pub use core_memory::*;

mod core_machine;
pub use core_machine::*;

mod core_crypto;
pub use core_crypto::*;

mod core_precompiles;
pub use core_crypto::{
    Precompile as CryptoPrecompile, PrecompileError as CryptoPrecompileError,
    PrecompileHalt as CryptoPrecompileHalt, PrecompileOutput as CryptoPrecompileOutput,
    PrecompileResult as CryptoPrecompileResult, PrecompileStatus as CryptoPrecompileStatus,
};
pub use core_memory::{AccountInfo, AccountState, ErasedError as DatabaseError};
#[cfg(feature = "std")]
pub use core_precompiles::AccountState as AccountConfigState;
pub use core_precompiles::{
    AccountInfo as PrecompileAccountInfo, ErasedError, Handler as StorageHandler, Precompile,
    PrecompileError, PrecompileHalt, PrecompileOutput, PrecompileResult, PrecompileStatus,
    Result as PrecompileExecutionResult, StorageKey as PrecompileStorageKey, *,
};
pub use core_primitives::{STACK_LIMIT, StorageKey};
pub use execution_handler::Handler;
