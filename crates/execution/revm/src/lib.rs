#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

mod api;
pub use api::{DefaultOp, DefaultOpEvm, OpBuilder, OpContext, OpContextTr, OpError};

mod constants;
pub use constants::{
    BASE_FEE_RECIPIENT, BASE_FEE_SCALAR_OFFSET, BLOB_BASE_FEE_SCALAR_OFFSET,
    DA_FOOTPRINT_GAS_SCALAR_OFFSET, DA_FOOTPRINT_GAS_SCALAR_SLOT, ECOTONE_L1_BLOB_BASE_FEE_SLOT,
    ECOTONE_L1_FEE_SCALARS_SLOT, EMPTY_SCALARS, L1_BASE_FEE_SLOT, L1_BLOCK_CONTRACT,
    L1_FEE_RECIPIENT, L1_OVERHEAD_SLOT, L1_SCALAR_SLOT, NON_ZERO_BYTE_COST,
    OPERATOR_FEE_CONSTANT_OFFSET, OPERATOR_FEE_JOVIAN_MULTIPLIER, OPERATOR_FEE_RECIPIENT,
    OPERATOR_FEE_SCALAR_DECIMAL, OPERATOR_FEE_SCALAR_OFFSET, OPERATOR_FEE_SCALARS_SLOT,
};

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
pub use spec::{OpSpecId, name};

mod transaction;
pub use transaction::{
    DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts, OpBuildError, OpTransaction,
    OpTransactionBuilder, OpTransactionError, OpTxTr, estimate_tx_compressed_size,
};

mod compat;
