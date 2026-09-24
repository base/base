#![doc = include_str!("../README.md")]

// The intrinsic-gas schedule and computation now live in the engine-neutral
// `base-common-eip8130` crate (shared with the EVM2 path); re-exported here so
// this crate's executor and its consumers keep a single import surface.
pub use base_common_eip8130::{
    AuthWireForm, Eip8130GasSchedule, IntrinsicGas, IntrinsicGasError, IntrinsicGasInput,
};

mod error;
pub use error::AuthError;

mod recovered;
pub use recovered::RecoveredActorId;

mod authorize;
pub use authorize::ActorAuthorizer;

mod tx_error;
pub use tx_error::TxAuthError;

mod verify;
pub use verify::{ActorTxVerifier, AuthorizedActor, TxActors};

mod nonce_error;
pub use nonce_error::NonceError;

mod validate;
pub use validate::{NonceMode, NonceStatus, NonceValidator};

mod apply;
pub use apply::{AppliedAccountChanges, ApplyError, DelegationEffect};

mod transaction;
pub use transaction::{AppliedTransaction, TransactionAuthorizer};

mod fee;
pub use fee::{FeeCheck, FeeError};
