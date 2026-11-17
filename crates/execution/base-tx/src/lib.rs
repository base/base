#![doc = include_str!("../README.md")]

// The intrinsic-gas schedule and computation now live in the engine-neutral
// `base-common-base-tx` crate (shared with the EVM2 path); re-exported here so
// this crate's executor and its consumers keep a single import surface.
pub use base_common_base_tx::{IntrinsicGas, TxBaseGasSchedule};

mod delegation;
pub use delegation::{DelegationEffect, DelegationError};

mod tx_error;
pub use tx_error::TxAuthError;

mod nonce_error;
pub use nonce_error::NonceError;

mod validate;
pub use validate::{NonceMode, NonceStatus, NonceValidator};

mod transaction;
pub use transaction::{AuthorizedTransaction, ResolvedSender, TransactionAuthorizer};

mod fee;
pub use fee::{FeeCheck, FeeError};

#[cfg(test)]
pub mod test_utils;
