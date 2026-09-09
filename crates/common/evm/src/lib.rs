#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use base_common_genesis::BaseUpgrade;

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
pub use base_common_precompiles::BasePrecompiles;

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

mod receipt_builder;
pub use receipt_builder::{AlloyReceiptBuilder, BaseReceiptBuilder};

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
