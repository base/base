#![doc = include_str!("../README.md")]

mod authorize;
pub use authorize::{AuthorizedTransaction, TransactionAuthorizer};
pub use base_execution_eip8130_authorize::ResolvedActor;
pub use base_execution_eip8130_tx::{TxActors, TxAuthError};
