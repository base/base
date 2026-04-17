#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod spec;
pub use spec::OpSpecId;

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
pub use precompiles::{
    BasePrecompiles, GRANITE, GRANITE_MAX_INPUT_SIZE, ISTHMUS_G1_MSM,
    ISTHMUS_G1_MSM_MAX_INPUT_SIZE, ISTHMUS_G2_MSM, ISTHMUS_G2_MSM_MAX_INPUT_SIZE, ISTHMUS_PAIRING,
    ISTHMUS_PAIRING_MAX_INPUT_SIZE, JOVIAN, JOVIAN_G1_MSM, JOVIAN_G1_MSM_MAX_INPUT_SIZE,
    JOVIAN_G2_MSM, JOVIAN_G2_MSM_MAX_INPUT_SIZE, JOVIAN_MAX_INPUT_SIZE, JOVIAN_PAIRING,
    JOVIAN_PAIRING_MAX_INPUT_SIZE, run_g1_msm_isthmus, run_g1_msm_jovian, run_g2_msm_isthmus,
    run_g2_msm_jovian, run_pair_granite, run_pair_jovian, run_pairing_isthmus, run_pairing_jovian,
};

mod api;
pub use api::{BaseError, Builder, DefaultOp, OpContext, OpContextTr};

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
