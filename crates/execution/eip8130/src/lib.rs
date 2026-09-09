#![doc = include_str!("../README.md")]

// The intrinsic-gas schedule and computation now live in the engine-neutral
// `base-common-eip8130` crate (shared with the EVM2 path); re-exported here so
// this crate's executor and its consumers keep a single import surface.
pub use base_common_eip8130::{
    AuthWireForm, Eip8130GasSchedule, IntrinsicGas, IntrinsicGasError, IntrinsicGasInput,
};

mod error;
pub use error::AuthError;

mod outcome;
pub use outcome::DispatchOutcome;

mod dispatch;
pub use dispatch::AuthenticatorDispatch;

mod account_config;
pub use account_config::{AccountConfigurationStorage, AccountState, ActorConfig, LockStatus};

mod authorize_error;
pub use authorize_error::AuthorizeError;

mod resolved;
pub use resolved::ResolvedActor;

mod recovered;
pub use recovered::RecoveredActorId;

mod authorize;
pub use authorize::ActorAuthorizer;

mod scope;
pub use scope::Operation;

mod tx_error;
pub use tx_error::TxAuthError;

mod verify;
pub use verify::{ActorTxVerifier, AuthorizedActor, TxActors};

mod signature;
pub use signature::{SignatureError, SignatureType, SignatureVerifier};

mod config;
pub use config::ConfigChangeAuthorizer;

mod nonce_error;
pub use nonce_error::NonceError;

mod validate;
pub use validate::{NonceMode, NonceStatus, NonceValidator};

mod events;
pub use events::{
    AccountConfigurationEvents, AccountCreated, ActorAuthorized, ActorRevoked, DelegationApplied,
};

mod apply;
pub use apply::{
    AccountChangeApplier, AppliedAccountChanges, ApplyError, CreatedAccount, DelegationEffect,
};

mod transaction;
pub use transaction::{AppliedTransaction, TransactionAuthorizer};

mod fee;
pub use fee::{FeeCheck, FeeError};
