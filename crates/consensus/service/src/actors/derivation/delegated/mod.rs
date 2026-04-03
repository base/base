//! Delegated derivation actor and its RPC client.

mod actor;
pub use actor::DelegateDerivationActor;

mod client;
pub use client::{DerivationDelegateClient, DerivationDelegateClientError};
