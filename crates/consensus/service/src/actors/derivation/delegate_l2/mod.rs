//! L2 delegation derivation actor and its RPC client.

mod actor;
pub use actor::DelegateL2DerivationActor;

mod client;
pub use client::{DelegateL2Client, DelegateL2ClientError, L2SourceClient};
