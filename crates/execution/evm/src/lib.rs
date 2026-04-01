#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

mod api;
pub use api::{DefaultOp, DefaultOpEvm, OpBuilder, OpContext, OpContextTr, OpError};

mod constants;
pub use constants::*;

mod core_evm;
pub use core_evm::OpRevmEvm;

mod handler;
pub use handler::{IsTxError, OpHandler};

mod l1block;
pub use l1block::L1BlockInfo;

mod precompiles;
pub use precompiles::{BasePrecompiles, bls12_381, bn254_pair};

mod result;
pub use result::OpHaltReason;

mod spec;
pub use spec::*;

mod transaction;
pub use transaction::{
    DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts, OpBuildError, OpTransaction,
    OpTransactionBuilder, OpTransactionError, OpTxTr,
};

mod compat;

#[cfg(feature = "reth")]
mod l1_block_error;
#[cfg(feature = "reth")]
pub use l1_block_error::{L1BlockInfoError, OpL1BlockError};

#[cfg(feature = "reth")]
mod next_block;
#[cfg(feature = "reth")]
pub use next_block::OpNextBlockEnvAttributes;

#[cfg(feature = "reth")]
mod l1_reth;
#[cfg(feature = "reth")]
pub use l1_reth::{
    RethL1BlockInfo, extract_l1_info, extract_l1_info_from_tx, parse_l1_info,
    parse_l1_info_tx_bedrock, parse_l1_info_tx_ecotone, parse_l1_info_tx_isthmus,
    parse_l1_info_tx_jovian,
};

#[cfg(feature = "alloy")]
mod evm;
#[cfg(feature = "alloy")]
pub use evm::OpEvm;

#[cfg(feature = "alloy")]
mod factory;
#[cfg(feature = "alloy")]
pub use factory::OpEvmFactory;

#[cfg(feature = "alloy")]
mod tx_env;
#[cfg(feature = "alloy")]
pub use tx_env::OpTxEnv;

#[cfg(feature = "alloy")]
mod ctx;
#[cfg(feature = "alloy")]
pub use ctx::OpBlockExecutionCtx;

#[cfg(feature = "alloy")]
mod error;
#[cfg(feature = "alloy")]
pub use error::OpBlockExecutionError;

#[cfg(feature = "alloy")]
mod receipt_builder;
#[cfg(feature = "alloy")]
pub use receipt_builder::{OpAlloyReceiptBuilder, OpReceiptBuilder};

#[cfg(feature = "alloy")]
mod canyon;
#[cfg(feature = "alloy")]
pub use canyon::ensure_create2_deployer;

#[cfg(feature = "alloy")]
mod executor;
#[cfg(feature = "alloy")]
pub use executor::{OpBlockExecutor, OpTxResult};

#[cfg(feature = "alloy")]
mod executor_factory;
#[cfg(feature = "alloy")]
pub use executor_factory::OpBlockExecutorFactory;

#[cfg(feature = "alloy")]
mod spec_id;
#[cfg(feature = "alloy")]
pub use spec_id::{spec, spec_by_timestamp_after_bedrock};

#[cfg(feature = "reth")]
mod receipts;
#[cfg(feature = "reth")]
pub use receipts::OpRethReceiptBuilder;

#[cfg(feature = "reth")]
mod build;
#[cfg(feature = "reth")]
pub use build::OpBlockAssembler;

#[cfg(feature = "reth")]
mod evm_config;
#[cfg(feature = "reth")]
pub use evm_config::{OpEvmConfig, OpExecutorProvider};
