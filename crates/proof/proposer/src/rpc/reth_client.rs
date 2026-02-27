//! Reth-specific L2 client implementation.
//!
//! Reth returns execution witnesses in a different format (arrays instead of maps).
//! This client handles the conversion and also populates block headers for BLOCKHASH
//! opcode support.

use std::collections::HashMap;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Header;
use async_trait::async_trait;
use backon::Retryable;
use base_enclave::{AccountResult, executor::ExecutionWitness};
use futures::stream::{self, StreamExt};

use super::{
    error::{RpcError, RpcResult},
    l2_client::{L2ClientConfig, L2ClientImpl},
    traits::L2Client,
    types::{OpBlock, RethExecutionWitness},
};

/// Reth-specific L2 client that wraps the standard L2 client.
///
/// This client handles the conversion from reth's array-based witness format
/// to the standard map-based format, and populates block headers for BLOCKHASH
/// opcode support.
pub struct RethL2Client {
    /// The inner L2 client.
    inner: L2ClientImpl,
}

impl std::fmt::Debug for RethL2Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RethL2Client").field("inner", &self.inner).finish()
    }
}

impl RethL2Client {
    /// Creates a new Reth L2 client from the given configuration.
    pub fn new(config: L2ClientConfig) -> RpcResult<Self> {
        Ok(Self { inner: L2ClientImpl::new(config)? })
    }

    /// Returns a reference to the inner L2 client.
    pub const fn inner(&self) -> &L2ClientImpl {
        &self.inner
    }

    /// Converts reth's array-based witness format to the standard map-based format.
    ///
    /// For codes: key = keccak256(bytecode), value = hex-encoded bytecode
    /// For state: key = keccak256(preimage), value = hex-encoded node content
    fn convert_reth_witness(reth_witness: RethExecutionWitness) -> ExecutionWitness {
        // Convert codes: compute keccak256 hash of each bytecode as the key
        let mut codes = HashMap::with_capacity(reth_witness.codes.len());
        for code in reth_witness.codes {
            let hash = keccak256(&code);
            let hex_key = format!("{hash:#x}");
            let hex_value = format!("0x{}", hex::encode(&code));
            codes.insert(hex_key, hex_value);
        }

        // Convert state: compute keccak256 hash of each preimage as the key
        let mut state = HashMap::with_capacity(reth_witness.state.len());
        for s in reth_witness.state {
            let hash = keccak256(&s);
            let hex_key = format!("{hash:#x}");
            let hex_value = format!("0x{}", hex::encode(&s));
            state.insert(hex_key, hex_value);
        }

        // Convert RPC headers to consensus headers (extract inner from RPC wrapper).
        let headers = reth_witness.headers.into_iter().map(|h| h.inner).collect();

        ExecutionWitness { state, codes, headers }
    }

    /// Validates that headers form a valid chain by checking `parent_hash` linkage.
    ///
    /// Headers are ordered with index 0 = block (n-1), index 1 = block (n-2), etc.
    /// For a valid chain: `headers[i].parent_hash == headers[i+1].hash()` for all i.
    fn validate_header_chain(headers: &[Header]) -> RpcResult<()> {
        if headers.len() <= 1 {
            return Ok(());
        }

        for i in 0..headers.len() - 1 {
            let current = &headers[i];
            let parent = &headers[i + 1];

            if current.inner.parent_hash != parent.hash {
                return Err(RpcError::HeaderChainInvalid(format!(
                    "Header at block {} has parent_hash {} but parent block {} has hash {}",
                    current.inner.number,
                    current.inner.parent_hash,
                    parent.inner.number,
                    parent.hash
                )));
            }
        }

        Ok(())
    }

    /// Populates block headers for BLOCKHASH opcode support if not already present.
    ///
    /// If the witness already has headers (from geth's `debug_executionWitness`
    /// response), they are used as-is. Otherwise, we fetch up to 256 parent
    /// headers (the EVM BLOCKHASH window) in parallel.
    async fn populate_headers(
        &self,
        block_number: u64,
        mut witness: ExecutionWitness,
    ) -> RpcResult<ExecutionWitness> {
        // If the witness already has headers from the node, use them.
        if !witness.headers.is_empty() {
            tracing::debug!(
                block_number,
                header_count = witness.headers.len(),
                "Using headers from witness response"
            );
            return Ok(witness);
        }

        // Otherwise, fetch up to 256 parent headers (for reth nodes that
        // don't include headers in the witness response).
        let history = std::cmp::min(256, block_number) as usize;

        if history == 0 {
            return Ok(witness);
        }

        tracing::debug!(block_number, history, "Fetching parent headers for BLOCKHASH support");

        let block_numbers: Vec<u64> =
            (1..=history as u64).map(|offset| block_number - offset).collect();

        // Fetch all headers in parallel with bounded concurrency
        // Using `buffered` instead of `buffer_unordered` to preserve order
        const CONCURRENCY: usize = 32;

        let results: Vec<RpcResult<Header>> = stream::iter(block_numbers)
            .map(|num| self.inner.header_by_number(Some(num)))
            .buffered(CONCURRENCY)
            .collect()
            .await;

        // Collect headers into temporary vector for validation
        // Index 0 = previous block (block_number - 1), as expected by ExecutionWitness
        let mut headers: Vec<Header> = Vec::with_capacity(results.len());
        for result in results {
            headers.push(result?);
        }

        // Validate header chain linkage before adding to witness
        Self::validate_header_chain(&headers)?;

        // Add validated headers to witness
        for header in headers {
            witness.headers.push(header.inner);
        }

        Ok(witness)
    }
}

#[async_trait]
impl L2Client for RethL2Client {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        self.inner.chain_config().await
    }

    async fn get_proof(&self, address: Address, block_hash: B256) -> RpcResult<AccountResult> {
        self.inner.get_proof(address, block_hash).await
    }

    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header> {
        self.inner.header_by_number(number).await
    }

    async fn block_by_number(&self, number: Option<u64>) -> RpcResult<OpBlock> {
        self.inner.block_by_number(number).await
    }

    async fn block_by_hash(&self, hash: B256) -> RpcResult<OpBlock> {
        self.inner.block_by_hash(hash).await
    }

    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness> {
        // Fetch the reth-format witness with retry
        let backoff = self.inner.retry_config().to_backoff_builder();

        let reth_witness: RethExecutionWitness = (|| async {
            self.inner
                .provider()
                .raw_request(
                    "debug_executionWitness".into(),
                    (BlockNumberOrTag::Number(block_number),),
                )
                .await
                .map_err(|e| RpcError::WitnessNotFound(format!("Block {block_number}: {e}")))
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying RethL2Client::execution_witness");
        })
        .await?;

        tracing::info!(
            block_number,
            witness_headers = reth_witness.headers.len(),
            witness_state = reth_witness.state.len(),
            witness_codes = reth_witness.codes.len(),
            "Received execution witness from node"
        );

        // Convert to standard format
        let witness = Self::convert_reth_witness(reth_witness);

        // Populate headers for BLOCKHASH support (skips if already present)
        self.populate_headers(block_number, witness).await
    }

    async fn db_get(&self, key: B256) -> RpcResult<Bytes> {
        self.inner.db_get(key).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_primitives::{B256, Bytes};

    use super::*;

    fn make_header(number: u64, hash: B256, parent_hash: B256) -> Header {
        Header {
            hash,
            inner: ConsensusHeader { number, parent_hash, ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn test_validate_header_chain_empty() {
        let headers: Vec<Header> = vec![];
        assert!(RethL2Client::validate_header_chain(&headers).is_ok());
    }

    #[test]
    fn test_validate_header_chain_single() {
        let header = make_header(100, B256::repeat_byte(1), B256::repeat_byte(0));
        assert!(RethL2Client::validate_header_chain(&[header]).is_ok());
    }

    #[test]
    fn test_validate_header_chain_valid() {
        // Headers in descending order: block 99, 98, 97
        // Block 99's parent_hash should be block 98's hash
        // Block 98's parent_hash should be block 97's hash
        let hash_99 = B256::repeat_byte(99);
        let hash_98 = B256::repeat_byte(98);
        let hash_97 = B256::repeat_byte(97);
        let hash_96 = B256::repeat_byte(96);

        let headers = vec![
            make_header(99, hash_99, hash_98), // index 0: block 99, parent is 98
            make_header(98, hash_98, hash_97), // index 1: block 98, parent is 97
            make_header(97, hash_97, hash_96), // index 2: block 97, parent is 96
        ];

        assert!(RethL2Client::validate_header_chain(&headers).is_ok());
    }

    #[test]
    fn test_validate_header_chain_invalid() {
        // Headers in descending order, but with broken linkage
        let hash_99 = B256::repeat_byte(99);
        let hash_98 = B256::repeat_byte(98);
        let hash_97 = B256::repeat_byte(97);
        let wrong_parent = B256::repeat_byte(0); // Wrong parent hash

        let headers = vec![
            make_header(99, hash_99, wrong_parent), // index 0: block 99, wrong parent
            make_header(98, hash_98, hash_97),      // index 1: block 98
            make_header(97, hash_97, B256::ZERO),   // index 2: block 97
        ];

        let result = RethL2Client::validate_header_chain(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, RpcError::HeaderChainInvalid(_)));
    }

    #[test]
    fn test_convert_reth_witness_codes() {
        let code = Bytes::from(vec![0x60, 0x80, 0x60, 0x40]); // Simple bytecode
        let expected_hash = keccak256(&code);

        let reth_witness = RethExecutionWitness {
            headers: vec![],
            codes: vec![code.clone()],
            state: vec![],
            keys: vec![],
        };

        let witness = RethL2Client::convert_reth_witness(reth_witness);

        assert_eq!(witness.codes.len(), 1);
        let hex_key = format!("{expected_hash:#x}");
        assert!(witness.codes.contains_key(&hex_key));
        assert_eq!(witness.codes.get(&hex_key), Some(&format!("0x{}", hex::encode(&code))));
    }

    #[test]
    fn test_convert_reth_witness_state() {
        let state_entry = Bytes::from(vec![0xab, 0xcd, 0xef]);
        let expected_hash = keccak256(&state_entry);

        let reth_witness = RethExecutionWitness {
            headers: vec![],
            codes: vec![],
            state: vec![state_entry.clone()],
            keys: vec![],
        };

        let witness = RethL2Client::convert_reth_witness(reth_witness);

        assert_eq!(witness.state.len(), 1);
        let hex_key = format!("{expected_hash:#x}");
        assert!(witness.state.contains_key(&hex_key));
        assert_eq!(witness.state.get(&hex_key), Some(&format!("0x{}", hex::encode(&state_entry))));
    }

    #[test]
    fn test_convert_reth_witness_empty() {
        let reth_witness =
            RethExecutionWitness { headers: vec![], codes: vec![], state: vec![], keys: vec![] };

        let witness = RethL2Client::convert_reth_witness(reth_witness);

        assert!(witness.codes.is_empty());
        assert!(witness.state.is_empty());
        assert!(witness.headers.is_empty());
    }
}
