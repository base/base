//! `TrieDB` provider implementation for stateless execution.
//!
//! This module provides a trie database implementation that uses pre-loaded
//! witness data for stateless block execution within an enclave.

use std::collections::HashMap;

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use base_proof_executor::TrieDBProvider;
use base_proof_mpt::{TrieNode, TrieProvider};

use super::witness::TransformedWitness;
use crate::error::ExecutorError;

/// Enclave `TrieDB` provider for stateless execution.
///
/// This provider uses pre-loaded witness data to provide bytecode and
/// state trie nodes during stateless block execution.
#[derive(Debug, Clone)]
pub struct EnclaveTrieDB {
    /// Bytecode map: `code_hash` -> bytecode.
    codes: HashMap<B256, Bytes>,

    /// State trie nodes: `node_hash` -> RLP-encoded node.
    state: HashMap<B256, Bytes>,

    /// The parent block header.
    parent_header: Header,

    /// Cached parent block hash (computed once on construction).
    parent_hash: B256,

    /// All witness headers indexed by hash, for BLOCKHASH opcode support.
    headers_by_hash: HashMap<B256, Header>,
}

impl EnclaveTrieDB {
    /// Creates a new `EnclaveTrieDB` from a transformed witness.
    #[must_use]
    pub fn from_witness(witness: TransformedWitness) -> Self {
        let parent_hash = witness.previous_header.hash_slow();
        Self {
            codes: witness.codes,
            state: witness.state,
            parent_header: witness.previous_header,
            parent_hash,
            headers_by_hash: witness.headers_by_hash,
        }
    }

    /// Creates a new `EnclaveTrieDB` with the given data.
    #[must_use]
    pub fn new(
        codes: HashMap<B256, Bytes>,
        state: HashMap<B256, Bytes>,
        parent_header: Header,
    ) -> Self {
        let parent_hash = parent_header.hash_slow();
        Self { codes, state, parent_header, parent_hash, headers_by_hash: HashMap::new() }
    }

    /// Returns bytecode by its hash.
    ///
    /// # Errors
    ///
    /// Returns `ExecutorError::ExecutionFailed` if the bytecode is not found.
    pub fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, ExecutorError> {
        self.codes.get(&hash).cloned().ok_or_else(|| {
            ExecutorError::ExecutionFailed(format!("bytecode not found for hash: {hash}"))
        })
    }

    /// Returns a header by its hash.
    ///
    /// Checks the parent header first, then falls back to the full headers map
    /// (populated from all witness headers for BLOCKHASH opcode support).
    ///
    /// # Errors
    ///
    /// Returns `ExecutorError::ExecutionFailed` if the header is not found.
    pub fn header_by_hash(&self, hash: B256) -> Result<Header, ExecutorError> {
        if hash == self.parent_hash {
            return Ok(self.parent_header.clone());
        }
        if let Some(header) = self.headers_by_hash.get(&hash) {
            return Ok(header.clone());
        }
        Err(ExecutorError::ExecutionFailed(format!("header not found for hash: {hash}")))
    }

    /// Returns a state trie node by its hash.
    ///
    /// # Errors
    ///
    /// Returns `ExecutorError::ExecutionFailed` if the node is not found.
    pub fn state_node_by_hash(&self, hash: B256) -> Result<Bytes, ExecutorError> {
        self.state.get(&hash).cloned().ok_or_else(|| {
            ExecutorError::ExecutionFailed(format!("state node not found for hash: {hash}"))
        })
    }

    /// Returns a reference to the parent header.
    #[must_use]
    pub const fn parent_header(&self) -> &Header {
        &self.parent_header
    }

    /// Returns the parent block hash.
    #[must_use]
    pub const fn parent_hash(&self) -> B256 {
        self.parent_hash
    }

    /// Returns the number of bytecode entries.
    #[must_use]
    pub fn codes_count(&self) -> usize {
        self.codes.len()
    }

    /// Returns the number of state trie nodes.
    #[must_use]
    pub fn state_nodes_count(&self) -> usize {
        self.state.len()
    }
}

/// Error type for `TrieProvider` and `TrieDBProvider` implementations.
#[derive(Debug, Clone)]
pub struct TrieProviderError(pub String);

impl std::fmt::Display for TrieProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TrieProviderError {}

/// Implementation of `TrieProvider` from base-proof-mpt for `EnclaveTrieDB`.
///
/// This enables the `EnclaveTrieDB` to be used as a trie node provider
/// for the stateless execution engine.
impl TrieProvider for EnclaveTrieDB {
    type Error = TrieProviderError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        // Look up the node in our state map
        let node_bytes = self
            .state
            .get(&key)
            .ok_or_else(|| TrieProviderError(format!("trie node not found for hash: {key}")))?;

        // Decode the RLP-encoded node into a TrieNode
        TrieNode::decode(&mut node_bytes.as_ref())
            .map_err(|e| TrieProviderError(format!("failed to decode trie node: {e}")))
    }
}

/// Implementation of `TrieDBProvider` from base-proof-executor for `EnclaveTrieDB`.
///
/// This enables the `EnclaveTrieDB` to provide bytecode and header lookups
/// required by the stateless L2 block executor.
impl TrieDBProvider for EnclaveTrieDB {
    fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes, Self::Error> {
        self.codes
            .get(&code_hash)
            .cloned()
            .ok_or_else(|| TrieProviderError(format!("bytecode not found for hash: {code_hash}")))
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        if hash == self.parent_hash {
            return Ok(self.parent_header.clone());
        }
        if let Some(header) = self.headers_by_hash.get(&hash) {
            return Ok(header.clone());
        }
        Err(TrieProviderError(format!("header not found for hash: {hash}")))
    }
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
    fn test_enclave_trie_db_new() {
        let header = test_header();
        let mut codes = HashMap::new();
        let code_hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        codes.insert(code_hash, Bytes::from_static(&[0x60, 0x80]));

        let trie_db = EnclaveTrieDB::new(codes, HashMap::new(), header.clone());

        assert_eq!(trie_db.codes_count(), 1);
        assert_eq!(trie_db.state_nodes_count(), 0);
        assert_eq!(trie_db.parent_header().number, header.number);
    }

    #[test]
    fn test_bytecode_by_hash_found() {
        let header = test_header();
        let mut codes = HashMap::new();
        let code_hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let bytecode = Bytes::from_static(&[0x60, 0x80, 0x60, 0x40, 0x52]);
        codes.insert(code_hash, bytecode.clone());

        let trie_db = EnclaveTrieDB::new(codes, HashMap::new(), header);

        let result = trie_db.bytecode_by_hash(code_hash);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), bytecode);
    }

    #[test]
    fn test_bytecode_by_hash_not_found() {
        let header = test_header();
        let trie_db = EnclaveTrieDB::new(HashMap::new(), HashMap::new(), header);

        let code_hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let result = trie_db.bytecode_by_hash(code_hash);
        assert!(matches!(result, Err(ExecutorError::ExecutionFailed(_))));
    }

    #[test]
    fn test_header_by_hash_found() {
        let header = test_header();
        let parent_hash = header.hash_slow();
        let trie_db = EnclaveTrieDB::new(HashMap::new(), HashMap::new(), header.clone());

        let result = trie_db.header_by_hash(parent_hash);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().number, header.number);
    }

    #[test]
    fn test_header_by_hash_not_found() {
        let header = test_header();
        let trie_db = EnclaveTrieDB::new(HashMap::new(), HashMap::new(), header);

        let wrong_hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let result = trie_db.header_by_hash(wrong_hash);
        assert!(matches!(result, Err(ExecutorError::ExecutionFailed(_))));
    }

    #[test]
    fn test_state_node_by_hash() {
        let header = test_header();
        let mut state = HashMap::new();
        let node_hash = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let node = Bytes::from_static(&[0xc0]);
        state.insert(node_hash, node.clone());

        let trie_db = EnclaveTrieDB::new(HashMap::new(), state, header);

        let result = trie_db.state_node_by_hash(node_hash);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), node);
    }
}
