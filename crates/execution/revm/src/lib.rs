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

mod fast_lz;

mod handler;
pub use handler::{IsTxError, OpHandler};

mod l1block;
pub use l1block::L1BlockInfo;

mod precompiles;
pub use precompiles::{OpPrecompiles, bls12_381, bn254_pair, fjord, granite, isthmus, jovian};

mod result;
pub use result::OpHaltReason;

mod spec;
pub use spec::*;

mod transaction;
pub use transaction::{
    DEPOSIT_TRANSACTION_TYPE,
    DepositTransactionParts,
    OpBuildError,
    OpTransaction,
    OpTransactionBuilder,
    OpTransactionError,
    OpTxTr,
    estimate_tx_compressed_size,
};

mod compat;
