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
    DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts, Eip8130Call, Eip8130CodePlacement,
    Eip8130Parts, Eip8130SequenceUpdate, Eip8130StorageWrite, account_address, create_bytecode,
    sequence_update,
};

mod handler;
pub use handler::{BaseHandler, IsTxError, delegation_code};

mod precompiles;
pub use precompiles::{
    BasePrecompileInstaller, BasePrecompiles, clear_eip8130_tx_context, extend_base_precompiles,
    install_base_precompiles, set_eip8130_tx_context,
};

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

mod base_v1;
pub use base_v1::ensure_aa_predeploys;

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseBlockExecutorFactory, BaseTxResult,
};
