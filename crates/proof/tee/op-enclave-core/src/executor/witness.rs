//! Execution witness types and transformation.
//!
//! This module provides types for execution witnesses used in stateless block execution,
//! matching the Go `stateless::ExecutionWitness` structure.

use std::collections::HashMap;

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes, keccak256};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::ExecutorError;

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

/// Transformed witness ready for use with kona-executor.
///
/// This is the processed form of `ExecutionWitness` with decoded bytes.
#[derive(Debug, Clone)]
pub struct TransformedWitness {
    /// The previous block header.
    pub previous_header: Header,

    /// Bytecode map: `code_hash` -> bytecode.
    pub codes: HashMap<B256, Bytes>,

    /// State trie nodes: `node_hash` -> RLP-encoded node.
    pub state: HashMap<B256, Bytes>,

    /// All witness headers indexed by hash, for BLOCKHASH opcode support.
    pub headers_by_hash: HashMap<B256, Header>,
}

impl TransformedWitness {
    /// Returns a reference to the previous block header.
    #[must_use]
    pub const fn previous_header(&self) -> &Header {
        &self.previous_header
    }

    /// Looks up bytecode by its hash.
    #[must_use]
    pub fn bytecode_by_hash(&self, hash: &B256) -> Option<&Bytes> {
        self.codes.get(hash)
    }

    /// Looks up a state trie node by its hash.
    #[must_use]
    pub fn state_node_by_hash(&self, hash: &B256) -> Option<&Bytes> {
        self.state.get(hash)
    }
}

/// Transform witness from JSON format to the format expected by kona-executor.
///
/// This performs the transformation described in `server.go:253-265`:
/// - Decode hex strings in `codes` map to `HashMap<B256, Bytes>`
/// - Decode hex strings in `state` map to `HashMap<B256, Bytes>`
/// - Extract `headers[0]` as previous block header
///
/// # Arguments
///
/// * `witness` - The raw execution witness from JSON
///
/// # Returns
///
/// A `TransformedWitness` with decoded data ready for execution.
///
/// # Errors
///
/// Returns `ExecutorError::WitnessTransformFailed` if:
/// - Headers is empty
/// - Any hex decoding fails
pub fn transform_witness(witness: ExecutionWitness) -> Result<TransformedWitness, ExecutorError> {
    // Extract previous header (headers[0])
    let previous_header = witness
        .headers
        .first()
        .ok_or_else(|| ExecutorError::WitnessTransformFailed("headers is empty".to_string()))?
        .clone();

    // Build a hash-indexed map of ALL headers for BLOCKHASH opcode support.
    // The EVM's BLOCKHASH opcode (and EIP-2935) needs to look up historical
    // block headers by their hash during execution.
    let mut headers_by_hash = HashMap::with_capacity(witness.headers.len());
    for header in &witness.headers {
        let hash = header.hash_slow();
        headers_by_hash.insert(hash, header.clone());
    }

    // Transform codes map
    let codes = transform_code_map(&witness.codes)?;

    // Transform state map
    let state = transform_state_map(&witness.state)?;

    // Witness integrity check: verify each state entry's key matches keccak256(value).
    // A mismatch means the witness provider sent corrupt trie node data.
    for (hash, node_bytes) in &state {
        let computed = keccak256(node_bytes);
        if computed != *hash {
            return Err(ExecutorError::WitnessTransformFailed(format!(
                "state node hash mismatch: key={hash}, computed={computed}, len={}",
                node_bytes.len()
            )));
        }
    }
    debug!(entries = state.len(), "Witness state integrity check passed");

    // Code integrity check: verify each code entry's key matches keccak256(value).
    for (hash, bytecode) in &codes {
        let computed = keccak256(bytecode);
        if computed != *hash {
            return Err(ExecutorError::WitnessTransformFailed(format!(
                "code hash mismatch: key={hash}, computed={computed}, len={}",
                bytecode.len()
            )));
        }
    }
    debug!(entries = codes.len(), "Witness code integrity check passed");

    Ok(TransformedWitness { previous_header, codes, state, headers_by_hash })
}

/// Transform the codes map from hex strings to decoded bytes.
///
/// The Go implementation in `transformMap` decodes the values and creates a set.
/// We maintain a hash -> bytecode mapping for lookup during execution.
fn transform_code_map(
    codes: &HashMap<String, String>,
) -> Result<HashMap<B256, Bytes>, ExecutorError> {
    let mut result = HashMap::with_capacity(codes.len());

    for (key, value) in codes {
        // Decode the key (code hash)
        let key_bytes = decode_hex(key)?;
        let code_hash = B256::try_from(key_bytes.as_slice()).map_err(|_| {
            ExecutorError::WitnessTransformFailed(format!(
                "invalid code hash length: expected 32, got {}",
                key_bytes.len()
            ))
        })?;

        // Decode the value (bytecode)
        let bytecode = Bytes::from(decode_hex(value)?);

        result.insert(code_hash, bytecode);
    }

    Ok(result)
}

/// Transform the state map from hex strings to decoded bytes.
fn transform_state_map(
    state: &HashMap<String, String>,
) -> Result<HashMap<B256, Bytes>, ExecutorError> {
    let mut result = HashMap::with_capacity(state.len());

    for (key, value) in state {
        // Decode the key (node hash)
        let key_bytes = decode_hex(key)?;
        let node_hash = B256::try_from(key_bytes.as_slice()).map_err(|_| {
            ExecutorError::WitnessTransformFailed(format!(
                "invalid node hash length: expected 32, got {}",
                key_bytes.len()
            ))
        })?;

        // Decode the value (RLP-encoded node)
        let node = Bytes::from(decode_hex(value)?);

        result.insert(node_hash, node);
    }

    Ok(result)
}

/// Decode a hex string (with or without 0x prefix) to bytes.
fn decode_hex(hex_str: &str) -> Result<Vec<u8>, ExecutorError> {
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    hex::decode(hex_str)
        .map_err(|e| ExecutorError::WitnessTransformFailed(format!("hex decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;

    use super::*;

    fn test_header() -> Header {
        Header {
            parent_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
            beneficiary: Default::default(),
            state_root: b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            transactions_root: b256!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            ),
            receipts_root: b256!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            ),
            logs_bloom: Default::default(),
            difficulty: Default::default(),
            number: 1,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: 1_700_000_000,
            extra_data: Default::default(),
            mix_hash: Default::default(),
            nonce: Default::default(),
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
        }
    }

    #[test]
    fn test_transform_witness_empty_headers() {
        let witness =
            ExecutionWitness { headers: vec![], codes: HashMap::new(), state: HashMap::new() };

        let result = transform_witness(witness);
        assert!(matches!(result, Err(ExecutorError::WitnessTransformFailed(_))));
    }

    #[test]
    fn test_transform_witness_success() {
        let header = test_header();

        // Use correct keccak256 hashes so integrity checks pass.
        let mut codes = HashMap::new();
        codes.insert(
            "0x1c3374235d773b2189aed115aa13143020fcdbbe86e38f358cf3e4771b2f0244".to_string(),
            "0x6080604052".to_string(),
        );

        let mut state = HashMap::new();
        state.insert(
            "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347".to_string(),
            "0xc0".to_string(),
        );

        let witness = ExecutionWitness { headers: vec![header.clone()], codes, state };

        let result = transform_witness(witness);
        assert!(result.is_ok());

        let transformed = result.unwrap();
        assert_eq!(transformed.previous_header.number, header.number);
        assert_eq!(transformed.codes.len(), 1);
        assert_eq!(transformed.state.len(), 1);

        // Verify code lookup by keccak256(code_bytes)
        let code_hash = b256!("1c3374235d773b2189aed115aa13143020fcdbbe86e38f358cf3e4771b2f0244");
        let code = transformed.bytecode_by_hash(&code_hash);
        assert!(code.is_some());
        assert_eq!(code.unwrap().as_ref(), &[0x60, 0x80, 0x60, 0x40, 0x52]);
    }

    #[test]
    fn test_decode_hex_with_prefix() {
        let result = decode_hex("0xabcd");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xab, 0xcd]);
    }

    #[test]
    fn test_decode_hex_without_prefix() {
        let result = decode_hex("abcd");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0xab, 0xcd]);
    }

    #[test]
    fn test_decode_hex_invalid() {
        let result = decode_hex("ghij");
        assert!(matches!(result, Err(ExecutorError::WitnessTransformFailed(_))));
    }
}
