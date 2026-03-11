//! Execution witness types.
//!
//! This module provides the `ExecutionWitness` type used in stateless block execution,
//! matching the Go `stateless::ExecutionWitness` structure.

use std::collections::HashMap;

use alloy_consensus::Header;
use serde::{Deserialize, Serialize};

/// Execution witness matching Go's `stateless::ExecutionWitness`.
///
/// This is the JSON representation received from the client, where codes and state
/// are hex-encoded string maps.
///
/// Uses `#[serde(alias)]` to accept both `PascalCase` (Go JSON encoding) and
/// camelCase/lowercase (geth `debug_executionWitness` RPC response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionWitness {
    /// Block headers (previous block header is at index 0).
    #[serde(alias = "Headers")]
    pub headers: Vec<Header>,

    /// Bytecode map: hex-encoded `code_hash` -> hex-encoded bytecode.
    #[serde(alias = "Codes")]
    pub codes: HashMap<String, String>,

    /// State trie nodes: hex-encoded `node_hash` -> hex-encoded RLP node.
    #[serde(alias = "State")]
    pub state: HashMap<String, String>,
}
