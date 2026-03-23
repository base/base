#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

mod api;
pub use api::{DefaultOp, DefaultOpEvm, OpBuilder, OpContext, OpContextTr, OpError};

mod constants;
pub use constants::*;

mod evm;
pub use evm::OpEvm;

mod handler;
pub use handler::{IsTxError, OpHandler};

mod l1block;
pub use l1block::L1BlockInfo;

mod precompiles;
pub use precompiles::{BasePrecompiles, bls12_381, bn254_pair};

mod result;
pub use result::OpHaltReason;

mod rollup_config;
pub use rollup_config::RollupConfigExt;

mod spec;
pub use spec::*;

mod transaction;
pub use transaction::{
    DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts, OpBuildError, OpTransaction,
    OpTransactionBuilder, OpTransactionError, OpTxTr,
};

mod compat;

#[cfg(feature = "execution")]
mod error;
#[cfg(feature = "execution")]
pub use error::{L1BlockInfoError, OpBlockExecutionError};

#[cfg(feature = "execution")]
mod next_block;
#[cfg(feature = "execution")]
pub use next_block::OpNextBlockEnvAttributes;

#[cfg(feature = "execution")]
mod l1_reth;
#[cfg(feature = "execution")]
pub use l1_reth::{
    RethL1BlockInfo, extract_l1_info, extract_l1_info_from_tx,
    parse_l1_info, parse_l1_info_tx_bedrock, parse_l1_info_tx_ecotone,
    parse_l1_info_tx_isthmus, parse_l1_info_tx_jovian,
};
