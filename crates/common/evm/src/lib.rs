#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod spec;
pub use spec::{OpSpecId, name};

mod constants;
pub use constants::*;

mod result;
pub use result::OpHaltReason;

mod l1block;
pub use l1block::L1BlockInfo;

mod transaction;
pub use transaction::{
    BaseTransactionBuilder, BuildError, DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts,
    OpTransaction, OpTransactionError, OpTxTr,
};

mod handler;
pub use handler::{IsTxError, OpHandler};

mod precompiles;
pub use precompiles::BasePrecompiles;

mod op_evm;
pub use op_evm::OpEvm;

mod api;
pub use api::{BaseError, Builder, DefaultOp, DefaultOpEvm, OpContext, OpContextTr};

mod compat;

mod consensus_compat;

mod evm;
pub use evm::BaseEvm;

mod factory;
pub use factory::BaseEvmFactory;

mod tx_env;
pub use tx_env::BaseTxEnv;

mod ctx;
pub use ctx::BaseBlockExecutionCtx;

mod error;
pub use error::BaseBlockExecutionError;

mod receipt_builder;
pub use receipt_builder::{AlloyReceiptBuilder, BaseReceiptBuilder};

mod canyon;
pub use canyon::ensure_create2_deployer;

mod executor;
pub use executor::{BaseBlockExecutor, BaseTxResult};

mod executor_factory;
pub use executor_factory::BaseBlockExecutorFactory;
