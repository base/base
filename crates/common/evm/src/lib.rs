#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use base_common_chains::BaseUpgrade;

mod spec;
pub use spec::BaseSpecId;

mod result;
pub use result::BaseHaltReason;

mod l1block;
pub use l1block::L1BlockInfo;

mod transaction;
pub use transaction::{
    BaseTransaction, BaseTransactionBuilder, BaseTransactionError, BaseTxTr, BuildError,
    DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts,
};

mod handler;
pub use handler::{BaseHandler, IsTxError};

mod precompiles;
pub use precompiles::BasePrecompiles;

mod api;
pub use api::{BaseContext, BaseContextTr, BaseError, Builder, DefaultBase};

mod evm;
pub use evm::BaseEvm;

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

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseBlockExecutorFactory, BaseTxResult,
};
