//! Transaction status API for Base Reth.
//!
//! This crate provides an RPC API for querying transaction status
//! from the local mempool or a remote sequencer.

/// RPC implementation for transaction status queries.
pub mod rpc;

use serde::{Deserialize, Serialize};

/// The status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum Status {
    /// Transaction is not known to the node.
    Unknown,
    /// Transaction is known to the node (in mempool or confirmed).
    Known,
}

/// Response containing the status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct TransactionStatusResponse {
    /// The status of the queried transaction.
    pub status: Status,
}
