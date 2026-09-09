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
pub use base_execution_evm_precompiles::BasePrecompiles;

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

mod rpc_eip8130;
pub use rpc_eip8130::{AUTHENTICATOR_SELECTOR_LEN, MAX_AUTH_SIZE, STUB_AUTH_FILL};

mod rpc_transaction;

pub use base_execution_evm_machine as interpreter;
pub use base_execution_evm_machine::{Context, Journal, JournalEntry};
pub use base_execution_evm_primitives as bytecode;
pub use base_execution_evm_primitives as primitives;
pub use base_execution_state_memory as database;
pub use base_execution_state_memory as state;
pub use base_execution_state_memory::{DatabaseCommit, DatabaseRef, NoopHook, OnStateHook};
pub use revm_precompile as precompile;
pub use revm_precompile::install_crypto;

mod execution_api;
pub use execution_api::*;

mod count_inspector;
pub use count_inspector::*;

#[cfg(feature = "tracer")]
mod eip3155;
#[cfg(feature = "tracer")]
pub use eip3155::*;

mod either;

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

mod gas;
pub use gas::*;

mod execution_handler;
pub use execution_handler::*;

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

pub use base_execution_evm_machine::{BlockEnvironment, EvmEnv, EvmLimitParams, TransactionEnvMut};

mod execution_error;
pub use execution_error::*;

mod tx;
pub use tx::*;

#[cfg(feature = "call-util")]
mod call;
#[cfg(feature = "call-util")]
pub use call::*;

#[cfg(feature = "overrides")]
mod overrides;
#[cfg(feature = "overrides")]
pub use overrides::*;

mod rpc;
pub use rpc::*;

mod tracing;
pub use tracing::*;

mod either_evm;

mod base_transactions;

pub use base_execution_evm_precompiles::{
    DynPrecompile, DynPrecompiles, ErasedError, EthPrecompiles, EvmInternals, EvmInternalsError,
    MovePrecompileError, Precompile, PrecompileInput, PrecompileLookup, PrecompileProvider,
    PrecompilesMap, TransactionTr, precompile_output_to_interpreter_result,
};
