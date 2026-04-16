#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

mod api;
pub use api::{DefaultOp, DefaultOpEvm, OpBuilder, OpContext, OpContextTr, OpError};

mod constants;
pub use constants::*;

mod eip8130_policy;
pub use eip8130_policy::{
    PendingOwnerState, PendingOwnerValidationError, owner_scope_allows,
    pending_owner_state_for_change, validate_pending_owner_state,
};

mod evm;
pub use evm::OpEvm;

mod handler;
pub use handler::{IsTxError, OpHandler};

mod l1block;
pub use l1block::L1BlockInfo;

mod precompiles;
pub use precompiles::{
    BasePrecompiles, Eip8130TxContext, NONCE_BASE_SLOT, NONCE_MANAGER_ADDRESS, NONCE_MANAGER_GAS,
    TX_CONTEXT_ADDRESS, TX_CONTEXT_GAS, aa_nonce_slot, base_v1, bls12_381, bn254_pair,
    clear_eip8130_tx_context, encode_address, encode_b256, encode_calls_abi, encode_u256, fjord,
    get_eip8130_tx_context, granite, isthmus, jovian, selector, set_eip8130_tx_context,
};

mod result;
pub use result::OpHaltReason;

mod rollup_config;
pub use rollup_config::RollupConfigExt;

mod spec;
pub use spec::*;

mod transaction;
pub use transaction::{
    DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts, Eip8130AuthorizerValidation, Eip8130Call,
    Eip8130CodePlacement, Eip8130ConfigLog, Eip8130ConfigOp, Eip8130DelegateNativeValidation,
    Eip8130Parts, Eip8130PhaseResult, Eip8130SequenceUpdate, Eip8130StorageWrite,
    Eip8130VerifyCall, OpBuildError, OpTransaction, OpTransactionBuilder, OpTransactionError,
    OpTxTr, config_log_to_system_log, decode_phase_statuses, encode_phase_statuses,
    extract_phase_statuses_from_logs, phase_statuses_log_topic, phase_statuses_system_log,
};

mod compat;
