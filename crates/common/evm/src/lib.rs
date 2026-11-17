#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use base_common_genesis::{BaseFeature, BaseUpgrade};

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
    BASE_TRANSACTION_TYPE, BaseTransaction, BaseTransactionBuilder, BaseTransactionError, BaseTxTr,
    BuildError, DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts, TxBaseExecutionMode,
    TxBaseTransactionParts,
};

mod handler;
pub use handler::{BaseHandler, IsTxError};

mod precompiles;
pub use precompiles::BasePrecompiles;

mod beryl_metrics;
pub use beryl_metrics::BerylPrecompileMetricsObserver;

mod api;
pub use api::{BaseContext, BaseContextTr, BaseError, Builder, DefaultBase};

mod evm;
pub use evm::BaseEvm;

#[cfg(feature = "std")]
mod base_tx;
#[cfg(feature = "std")]
pub use base_tx::{TxBaseExecutor, TxBaseOutcome};

mod base_tx_committed_phases;
pub use base_tx_committed_phases::TxBaseCommittedPhases;

mod metered_gas;
pub use metered_gas::MeteredGas;

mod factory;
pub use factory::BaseEvmFactory;

mod tx_env;
pub use tx_env::BaseTxEnv;

mod error;
pub use error::BaseBlockExecutionError;

mod receipt_builder;
pub use receipt_builder::{AlloyReceiptBuilder, BaseReceiptBuilder};

mod canyon;
pub use canyon::ensure_create2_deployer;

mod everest;
pub use everest::ensure_base_tx_system_accounts;

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseBlockExecutorFactory, BaseTxResult,
};
