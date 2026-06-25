//! Follow-mode runtime, clients, and RPC surface.

mod engine;
mod error;
pub use error::FollowError;

mod local;
mod node;
pub use node::{FollowNode, FollowNodeConfig};

mod prefetcher;
mod proof_gate;
mod rpc;
mod runtime;
mod source;

#[cfg(test)]
pub use source::MockRemoteClient;
pub use source::{RemoteClient, RemoteL2Client, RemoteL2ClientError};
