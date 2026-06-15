#![doc = include_str!("../README.md")]

mod error;
pub use error::TxAuthError;

mod scope;
pub use scope::Operation;

mod verify;
pub use verify::{ActorTxVerifier, AuthorizedActor, TxActors};

mod config;
pub use config::ConfigChangeAuthorizer;
