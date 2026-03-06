//! TEE-based nullification proof generation for the challenger.
//!
//! Re-executes a range of intermediate blocks inside a TEE to produce a
//! nullification proof, proving that a claimed output root is wrong.
//! Delegates block proving and aggregation to the shared [`RangeProver`].
//!
//! ## Proof format
//!
//! The 130-byte output matches the `AggregateVerifier` contract interface:
//!
//! | Offset | Length | Field |
//! |--------|--------|-------|
//! | 0      | 1      | Proof type (`0x00` = TEE) |
//! | 1      | 32     | L1 origin block hash |
//! | 33     | 32     | L1 origin block number (big-endian `uint256`) |
//! | 65     | 65     | ECDSA signature (v-value adjusted to 27/28) |
//!
//! ## Availability
//!
//! TEE proof generation is gated behind the optional `--tee-endpoint` CLI
//! flag (`CHALLENGER_TEE_ENDPOINT`). When the endpoint is not configured,
//! the orchestrator skips TEE and falls back to ZK proof generation.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes};
use base_proof_rpc::{L1Provider, RollupProvider, RpcError};
use base_tee_prover::{
    ExecutionWitnessProvider, ProofEncoder, RangeProver, RangeProverError, TeeExecutor,
};
use thiserror::Error;
use tracing::info;

/// Errors that can occur during TEE proof generation.
#[derive(Debug, Error)]
pub enum TeeProofError {
    /// Range prover failed.
    #[error(transparent)]
    RangeProver(#[from] RangeProverError),

    /// RPC data fetch failed.
    #[error("failed to fetch {context}: {source}")]
    Rpc {
        /// Description of the fetch operation.
        context: &'static str,
        /// The underlying RPC error.
        source: RpcError,
    },

    /// Non-RPC data preparation failed (arithmetic overflows, missing data, derivation errors).
    #[error("data preparation failed: {0}")]
    DataPrep(String),

    /// Proof encoding or data transformation failed.
    #[error("proof encoding failed: {0}")]
    Encoding(String),
}

impl TeeProofError {
    /// Returns `true` if the error is transient and the operation can be retried.
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::RangeProver(e) => e.is_retryable(),
            Self::Rpc { source, .. } => source.is_retryable(),
            Self::DataPrep(_) | Self::Encoding(_) => false,
        }
    }
}

/// Generates TEE-based nullification proofs for invalid candidate games.
///
/// Re-executes a range of intermediate blocks inside a TEE to produce a
/// proof that the claimed output root at that checkpoint is wrong.
#[derive(Debug)]
pub struct TeeProofGenerator<E, L1, L2, R> {
    /// Enclave client for stateless block execution.
    enclave_client: Arc<E>,
    /// L1 RPC provider.
    l1_provider: Arc<L1>,
    /// L2 RPC provider (with execution witness support).
    l2_provider: Arc<L2>,
    /// Rollup RPC provider for configuration.
    rollup_provider: Arc<R>,
    /// Keccak256 hash of the TEE image PCR0, used to identify the enclave
    /// image that will produce the proof.
    tee_image_hash: B256,
}

impl<E, L1, L2, R> TeeProofGenerator<E, L1, L2, R>
where
    E: TeeExecutor,
    L1: L1Provider,
    L2: ExecutionWitnessProvider,
    R: RollupProvider,
{
    /// Creates a new TEE proof generator.
    #[must_use]
    pub const fn new(
        enclave_client: Arc<E>,
        l1_provider: Arc<L1>,
        l2_provider: Arc<L2>,
        rollup_provider: Arc<R>,
        tee_image_hash: B256,
    ) -> Self {
        Self { enclave_client, l1_provider, l2_provider, rollup_provider, tee_image_hash }
    }

    /// Generates a TEE nullification proof for an invalid intermediate checkpoint.
    ///
    /// Proves every block in the intermediate interval and aggregates the
    /// results, matching the format the `AggregateVerifier` contract expects.
    ///
    /// # Arguments
    ///
    /// * `game` - The candidate game containing the invalid checkpoint
    /// * `invalid_index` - Zero-based index of the first invalid intermediate root
    /// * `intermediate_block_interval` - Number of blocks between checkpoints
    /// * `prev_output_root` - The output root at the block before the interval starts
    ///
    /// # Returns
    ///
    /// 130-byte proof bytes: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)`
    ///
    /// # Errors
    ///
    /// Returns [`TeeProofError`] if data fetching, enclave execution, or encoding fails.
    pub async fn generate_tee_proof(
        &self,
        game: &crate::CandidateGame,
        invalid_index: usize,
        intermediate_block_interval: u64,
        prev_output_root: B256,
    ) -> Result<Bytes, TeeProofError> {
        let prev_block_number = u64::try_from(invalid_index)
            .ok()
            .and_then(|i| i.checked_mul(intermediate_block_interval))
            .and_then(|offset| game.starting_block_number.checked_add(offset))
            .ok_or_else(|| {
                TeeProofError::DataPrep("arithmetic overflow computing prev block".into())
            })?;

        info!(
            prev_block = %prev_block_number,
            block_count = %intermediate_block_interval,
            game_index = %game.index,
            invalid_index = %invalid_index,
            "generating TEE proof"
        );

        // Fetch the rollup config
        let rollup_config = self
            .rollup_provider
            .rollup_config()
            .await
            .map_err(|e| TeeProofError::Rpc { context: "rollup config", source: e })?;

        let range_prover = RangeProver::new(
            rollup_config,
            Arc::clone(&self.l1_provider),
            Arc::clone(&self.l2_provider),
            Arc::clone(&self.enclave_client),
            Address::ZERO,
            self.tee_image_hash,
            "base-challenger",
        )?;

        let proposal = range_prover
            .prove_range(prev_block_number, intermediate_block_interval, prev_output_root, vec![])
            .await?;

        let l1_origin_number: u64 = proposal
            .l1_origin_number
            .try_into()
            .map_err(|_| TeeProofError::DataPrep("L1 origin number overflows u64".into()))?;

        let proof_bytes = ProofEncoder::encode_proof_bytes(
            &proposal.signature,
            proposal.l1_origin_hash,
            l1_origin_number,
        )
        .map_err(|e| TeeProofError::Encoding(e.to_string()))?;

        info!(
            prev_block = %prev_block_number,
            block_count = %intermediate_block_interval,
            game_index = %game.index,
            proof_len = %proof_bytes.len(),
            "TEE proof generated"
        );

        Ok(proof_bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use alloy_consensus::{
        Header as ConsensusHeader, Receipt, ReceiptWithBloom, Sealable, Signed, TxEip1559,
    };
    use alloy_primitives::{
        Address, LogData, Signature as PrimitiveSignature, TxKind, U256, address, b256,
    };
    use alloy_rpc_types_eth::{Block, BlockTransactions, Header as RpcHeader, TransactionReceipt};
    use async_trait::async_trait;
    use base_alloy_consensus::{OpTxEnvelope, TxDeposit};
    use base_enclave::{
        AccountResult, AggregateRequest, ExecutionWitness, Proposal, RollupConfig,
        default_rollup_config,
    };
    use base_enclave_client::{ClientError, ExecuteStatelessRequest};
    use base_proof_contracts::{GameAtIndex, GameInfo};
    use base_proof_rpc::{L2Provider, OpBlock, RollupProvider, RpcResult};
    use base_protocol::{L1BlockInfoBedrock, Predeploys};
    use base_tee_prover::{ConfigBuilder, PROOF_TYPE_TEE, ReceiptConverter, TransactionSerializer};

    use super::*;
    use crate::CandidateGame;

    /// Concrete type alias for tests so associated functions are callable.
    type TestGenerator = TeeProofGenerator<
        MockEnclaveClient,
        MockL1Provider,
        MockChallengerL2Provider,
        MockRollupProvider,
    >;

    // ========================================================================
    // Mock types
    // ========================================================================

    /// Mock enclave client for testing.
    ///
    /// Optionally captures the last request for assertion in integration tests.
    #[derive(Debug)]
    struct MockEnclaveClient {
        result: Result<Proposal, ClientError>,
        captured_request: Mutex<Option<ExecuteStatelessRequest>>,
    }

    impl MockEnclaveClient {
        /// Creates a mock that returns the given result without capturing.
        fn new(result: Result<Proposal, ClientError>) -> Self {
            Self { result, captured_request: Mutex::new(None) }
        }
    }

    #[async_trait]
    impl TeeExecutor for MockEnclaveClient {
        async fn execute_stateless(
            &self,
            req: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            *self.captured_request.lock().unwrap() = Some(req);
            match &self.result {
                Ok(p) => Ok(p.clone()),
                Err(e) => Err(ClientError::ClientCreation(e.to_string())),
            }
        }

        async fn aggregate(&self, _req: AggregateRequest) -> Result<Proposal, ClientError> {
            match &self.result {
                Ok(p) => Ok(p.clone()),
                Err(e) => Err(ClientError::ClientCreation(e.to_string())),
            }
        }
    }

    /// Mock L1 provider.
    #[derive(Debug)]
    struct MockL1Provider {
        headers: HashMap<B256, RpcHeader>,
        receipts: HashMap<B256, Vec<TransactionReceipt>>,
    }

    #[async_trait]
    impl L1Provider for MockL1Provider {
        async fn block_number(&self) -> RpcResult<u64> {
            Ok(0)
        }

        async fn header_by_number(&self, _number: Option<u64>) -> RpcResult<RpcHeader> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }

        async fn header_by_hash(&self, hash: B256) -> RpcResult<RpcHeader> {
            self.headers
                .get(&hash)
                .cloned()
                .ok_or_else(|| RpcError::HeaderNotFound(format!("no header for {hash}")))
        }

        async fn block_receipts(&self, hash: B256) -> RpcResult<Vec<TransactionReceipt>> {
            self.receipts
                .get(&hash)
                .cloned()
                .ok_or_else(|| RpcError::BlockNotFound(format!("no receipts for {hash}")))
        }

        async fn code_at(&self, _address: Address, _block_number: Option<u64>) -> RpcResult<Bytes> {
            Ok(Bytes::new())
        }

        async fn call_contract(
            &self,
            _to: Address,
            _data: Bytes,
            _block_number: Option<u64>,
        ) -> RpcResult<Bytes> {
            Ok(Bytes::new())
        }

        async fn get_balance(&self, _address: Address) -> RpcResult<U256> {
            Ok(U256::ZERO)
        }
    }

    /// Mock L2 provider with execution witness support.
    #[derive(Debug)]
    struct MockChallengerL2Provider {
        blocks: HashMap<u64, OpBlock>,
        blocks_by_hash: HashMap<B256, OpBlock>,
        proofs: HashMap<B256, AccountResult>,
        witness: Option<ExecutionWitness>,
        error: Option<String>,
    }

    impl MockChallengerL2Provider {
        fn new() -> Self {
            Self {
                blocks: HashMap::new(),
                blocks_by_hash: HashMap::new(),
                proofs: HashMap::new(),
                witness: None,
                error: None,
            }
        }
    }

    #[async_trait]
    impl L2Provider for MockChallengerL2Provider {
        async fn chain_config(&self) -> RpcResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }

        async fn get_proof(&self, _address: Address, block_hash: B256) -> RpcResult<AccountResult> {
            self.proofs
                .get(&block_hash)
                .cloned()
                .ok_or_else(|| RpcError::ProofNotFound(format!("no proof for {block_hash}")))
        }

        async fn header_by_number(&self, _number: Option<u64>) -> RpcResult<RpcHeader> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }

        async fn block_by_number(&self, number: Option<u64>) -> RpcResult<OpBlock> {
            if let Some(err) = &self.error {
                return Err(RpcError::BlockNotFound(err.clone()));
            }
            let n = number.unwrap_or(0);
            self.blocks
                .get(&n)
                .cloned()
                .ok_or_else(|| RpcError::BlockNotFound(format!("block {n} not found")))
        }

        async fn block_by_hash(&self, hash: B256) -> RpcResult<OpBlock> {
            self.blocks_by_hash
                .get(&hash)
                .cloned()
                .ok_or_else(|| RpcError::BlockNotFound(format!("block {hash} not found")))
        }
    }

    #[async_trait]
    impl ExecutionWitnessProvider for MockChallengerL2Provider {
        async fn execution_witness(&self, _block_number: u64) -> RpcResult<ExecutionWitness> {
            self.witness.clone().ok_or_else(|| RpcError::BlockNotFound("no witness".into()))
        }
    }

    /// Mock rollup provider.
    #[derive(Debug)]
    struct MockRollupProvider {
        config: Option<RollupConfig>,
    }

    #[async_trait]
    impl RollupProvider for MockRollupProvider {
        async fn rollup_config(&self) -> RpcResult<RollupConfig> {
            self.config.clone().ok_or_else(|| RpcError::BlockNotFound("no config".into()))
        }

        async fn sync_status(&self) -> RpcResult<base_proof_rpc::SyncStatus> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }
    }

    fn test_candidate_game(starting_block: u64) -> CandidateGame {
        let proxy = address!("0000000000000000000000000000000000000001");
        CandidateGame {
            index: 42,
            factory: GameAtIndex { game_type: 1, timestamp: 1_000_000, proxy },
            info: GameInfo {
                root_claim: B256::repeat_byte(0xAA),
                l2_block_number: starting_block.saturating_add(100),
                parent_index: 0,
            },
            starting_block_number: starting_block,
        }
    }

    /// Creates a deposit transaction carrying L1 block info (Bedrock format).
    fn make_deposit_tx(l1_hash: B256, l1_number: u64) -> base_alloy_rpc_types::Transaction {
        let l1_info = L1BlockInfoBedrock::new(
            l1_number,
            1_700_000_000, // timestamp
            1_000_000_000, // base_fee
            l1_hash,
            0,                  // sequence_number
            Address::ZERO,      // batcher_address
            U256::ZERO,         // l1_fee_overhead
            U256::from(684000), // l1_fee_scalar
        );
        let calldata = base_protocol::L1BlockInfoTx::Bedrock(l1_info).encode_calldata();

        let deposit = TxDeposit {
            source_hash: B256::repeat_byte(0x01),
            from: address!("DeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: true,
            input: calldata,
        };

        let sealed = deposit.seal_slow();
        let envelope = OpTxEnvelope::Deposit(sealed);

        base_alloy_rpc_types::Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: alloy_consensus::transaction::Recovered::new_unchecked(
                    envelope,
                    Address::ZERO,
                ),
                block_hash: None,
                block_number: None,
                transaction_index: Some(0),
                effective_gas_price: Some(0),
            },
            deposit_nonce: Some(0),
            deposit_receipt_version: None,
        }
    }

    /// Builds a minimal `OpBlock` containing only a deposit transaction.
    fn make_test_block(
        block_number: u64,
        parent_hash: B256,
        l1_origin_hash: B256,
        l1_origin_number: u64,
    ) -> OpBlock {
        let deposit_tx = make_deposit_tx(l1_origin_hash, l1_origin_number);
        let consensus_header = ConsensusHeader {
            parent_hash,
            number: block_number,
            timestamp: 1_700_000_000 + block_number * 2,
            ..Default::default()
        };
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };

        Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Full(vec![deposit_tx]),
            withdrawals: None,
        }
    }

    /// Creates a minimal `AccountResult` for testing.
    fn mock_account_result() -> AccountResult {
        AccountResult {
            address: Predeploys::L2_TO_L1_MESSAGE_PASSER,
            account_proof: vec![],
            balance: U256::ZERO,
            code_hash: b256!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"),
            nonce: U256::ZERO,
            storage_hash: B256::repeat_byte(0x01),
            storage_proof: vec![],
        }
    }

    /// Builds fully-wired test fixtures: mock providers populated with
    /// consistent block, header, proof, and witness data.
    fn wired_providers(
        target_block_number: u64,
    ) -> (MockChallengerL2Provider, MockL1Provider, MockRollupProvider, B256) {
        let l1_origin_hash = B256::repeat_byte(0xEE);
        let l1_origin_number = 500u64;

        let parent_hash = B256::repeat_byte(0x22);
        let target_block =
            make_test_block(target_block_number, parent_hash, l1_origin_hash, l1_origin_number);
        let target_hash = target_block.header.hash;

        let prev_block = make_test_block(
            target_block_number - 1,
            B256::repeat_byte(0x33),
            l1_origin_hash,
            l1_origin_number,
        );

        let account = mock_account_result();
        let witness = ExecutionWitness {
            headers: vec![prev_block.header.inner.clone()],
            codes: HashMap::new(),
            state: HashMap::new(),
        };

        let l1_header = RpcHeader {
            hash: l1_origin_hash,
            inner: ConsensusHeader { number: l1_origin_number, ..Default::default() },
            ..Default::default()
        };

        let mut l2 = MockChallengerL2Provider::new();
        l2.blocks.insert(target_block_number, target_block);
        l2.blocks_by_hash.insert(parent_hash, prev_block);
        l2.proofs.insert(target_hash, account.clone());
        l2.proofs.insert(parent_hash, account);
        l2.witness = Some(witness);

        let mut l1_headers = HashMap::new();
        l1_headers.insert(l1_origin_hash, l1_header);
        let mut l1_receipts = HashMap::new();
        l1_receipts.insert(l1_origin_hash, vec![]);

        let l1 = MockL1Provider { headers: l1_headers, receipts: l1_receipts };
        let rollup = MockRollupProvider { config: Some(default_rollup_config()) };

        (l2, l1, rollup, l1_origin_hash)
    }

    // ========================================================================
    // Proof encoding tests (delegate to shared crate, tested there)
    // ========================================================================

    fn test_proposal(l1_hash: B256, l1_number: u64) -> Proposal {
        let mut sig = vec![0xAB; 65];
        sig[64] = 0;
        Proposal {
            output_root: B256::repeat_byte(0x11),
            signature: Bytes::from(sig),
            l1_origin_hash: l1_hash,
            l1_origin_number: U256::from(l1_number),
            l2_block_number: U256::from(100u64),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        }
    }

    // ========================================================================
    // Target block overflow tests
    // ========================================================================

    fn generator_with_no_data() -> TestGenerator {
        TeeProofGenerator::new(
            Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0)))),
            Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() }),
            Arc::new(MockChallengerL2Provider::new()),
            Arc::new(MockRollupProvider { config: None }),
            B256::ZERO,
        )
    }

    #[tokio::test]
    async fn test_generate_tee_proof_index_overflow() {
        let generator = generator_with_no_data();
        let game = test_candidate_game(0);

        // index * interval overflows: usize::MAX * 2
        let result = generator.generate_tee_proof(&game, usize::MAX, 2, B256::ZERO).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::DataPrep(_)));
        assert!(err.to_string().contains("arithmetic overflow"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_mul_overflow() {
        let generator = generator_with_no_data();
        let game = test_candidate_game(0);

        // index * interval overflows: 2 * (u64::MAX / 2 + 1)
        let result = generator.generate_tee_proof(&game, 2, u64::MAX / 2 + 1, B256::ZERO).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::DataPrep(_)));
        assert!(err.to_string().contains("arithmetic overflow"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_add_overflow() {
        let generator = generator_with_no_data();
        let game = test_candidate_game(u64::MAX - 50);

        // index * interval + starting_block overflows: 1 * 100 + (u64::MAX - 50)
        let result = generator.generate_tee_proof(&game, 1, 100, B256::ZERO).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::DataPrep(_)));
        assert!(err.to_string().contains("arithmetic overflow"));
    }

    // ========================================================================
    // Transaction serialization tests
    // ========================================================================

    fn make_eip1559_tx() -> base_alloy_rpc_types::Transaction {
        let tx = TxEip1559 { chain_id: 1, gas_limit: 21000, ..Default::default() };
        let sig = PrimitiveSignature::new(U256::from(1), U256::from(2), false);
        let signed = Signed::new_unchecked(tx, sig, B256::ZERO);
        let envelope = OpTxEnvelope::Eip1559(signed);

        base_alloy_rpc_types::Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: alloy_consensus::transaction::Recovered::new_unchecked(
                    envelope,
                    Address::ZERO,
                ),
                block_hash: None,
                block_number: None,
                transaction_index: Some(1),
                effective_gas_price: Some(0),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    #[test]
    fn test_serialize_block_transactions_deposit_filtering() {
        let deposit_tx = make_deposit_tx(B256::ZERO, 100);
        let eip1559_tx = make_eip1559_tx();
        let consensus_header = ConsensusHeader { number: 1, ..Default::default() };
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };

        let block = Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Full(vec![deposit_tx, eip1559_tx]),
            withdrawals: None,
        };

        let all_txs = TransactionSerializer::serialize_block_transactions(&block, true).unwrap();
        assert_eq!(all_txs.len(), 2);

        let sequenced_txs =
            TransactionSerializer::serialize_block_transactions(&block, false).unwrap();
        assert_eq!(sequenced_txs.len(), 1);
        assert_eq!(sequenced_txs[0][0], 0x02);
    }

    // ========================================================================
    // Error type tests
    // ========================================================================

    #[test]
    fn test_tee_proof_error_rpc_display() {
        let err = TeeProofError::Rpc {
            context: "rollup config",
            source: RpcError::Timeout("rpc timeout".into()),
        };
        assert_eq!(err.to_string(), "failed to fetch rollup config: Request timeout: rpc timeout");
    }

    #[test]
    fn test_tee_proof_error_data_prep_display() {
        let err = TeeProofError::DataPrep("arithmetic overflow".into());
        assert_eq!(err.to_string(), "data preparation failed: arithmetic overflow");
    }

    #[test]
    fn test_is_retryable_rpc_transport() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::Transport("connection reset".into()),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_rpc_timeout() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::Timeout("timed out".into()),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_rpc_connection() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::Connection("refused".into()),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_rpc_block_not_found() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::BlockNotFound("block 123 not found".into()),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_data_prep() {
        let err = TeeProofError::DataPrep("arithmetic overflow".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_retryable_range_prover_rpc() {
        let inner = RangeProverError::Rpc {
            context: "target block",
            source: RpcError::Transport("connection reset".into()),
        };
        let err = TeeProofError::RangeProver(inner);
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_range_prover_empty() {
        let err = TeeProofError::RangeProver(RangeProverError::Empty);
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_encoding() {
        let err = TeeProofError::Encoding("bad bytes".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_tee_proof_error_encoding_display() {
        let err = TeeProofError::Encoding("bad bytes".into());
        assert_eq!(err.to_string(), "proof encoding failed: bad bytes");
    }

    // ========================================================================
    // Integration-style tests for generate_tee_proof
    // ========================================================================

    #[tokio::test]
    async fn test_generate_tee_proof_rollup_config_missing() {
        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0))));
        let l1 = Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() });
        let l2 = Arc::new(MockChallengerL2Provider::new());
        let rollup = Arc::new(MockRollupProvider { config: None });

        let generator = TeeProofGenerator::new(enclave, l1, l2, rollup, B256::ZERO);
        let game = test_candidate_game(100);

        let result = generator.generate_tee_proof(&game, 0, 10, B256::ZERO).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::Rpc { .. }));
        assert!(err.to_string().contains("rollup config"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_block_fetch_error() {
        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0))));
        let l1 = Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() });
        let l2 = Arc::new(MockChallengerL2Provider {
            error: Some("node unavailable".into()),
            ..MockChallengerL2Provider::new()
        });
        let rollup = Arc::new(MockRollupProvider { config: Some(default_rollup_config()) });

        let generator = TeeProofGenerator::new(enclave, l1, l2, rollup, B256::ZERO);
        let game = test_candidate_game(100);

        let result = generator.generate_tee_proof(&game, 0, 10, B256::ZERO).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::RangeProver(RangeProverError::Rpc { .. })));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_empty_block() {
        let target_block_number = 110u64;
        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0))));
        let l1 = Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() });

        let consensus_header =
            ConsensusHeader { number: target_block_number, ..Default::default() };
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };
        let empty_block = Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Full(vec![]),
            withdrawals: None,
        };

        let mut l2 = MockChallengerL2Provider::new();
        l2.blocks.insert(target_block_number, empty_block);

        let rollup = Arc::new(MockRollupProvider { config: Some(default_rollup_config()) });
        let generator = TeeProofGenerator::new(enclave, l1, Arc::new(l2), rollup, B256::ZERO);
        let game = test_candidate_game(100);

        // interval=1, index=9 => prev_block=109, proves only block 110 (the empty one)
        let result = generator.generate_tee_proof(&game, 9, 1, B256::ZERO).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::RangeProver(RangeProverError::DataPrep(_))));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_enclave_error() {
        let target_block_number = 110u64;
        let (l2, l1, rollup, _) = wired_providers(target_block_number);

        let enclave = Arc::new(MockEnclaveClient::new(Err(ClientError::ClientCreation(
            "enclave down".into(),
        ))));

        let generator = TeeProofGenerator::new(
            enclave,
            Arc::new(l1),
            Arc::new(l2),
            Arc::new(rollup),
            B256::ZERO,
        );
        let game = test_candidate_game(100);

        // interval=1, index=9 => prev_block=109, proves only block 110
        let result = generator.generate_tee_proof(&game, 9, 1, B256::ZERO).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, TeeProofError::RangeProver(RangeProverError::Enclave(_))),
            "expected Enclave error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_generate_tee_proof_happy_path() {
        let target_block_number = 110u64;
        let (l2, l1, rollup, l1_origin_hash) = wired_providers(target_block_number);

        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(l1_origin_hash, 500))));

        let generator = TeeProofGenerator::new(
            Arc::clone(&enclave),
            Arc::new(l1),
            Arc::new(l2),
            Arc::new(rollup),
            B256::ZERO,
        );
        let game = test_candidate_game(100);

        // interval=1, invalid_index=9 => prev_block=109, proves block 110
        let result = generator.generate_tee_proof(&game, 9, 1, B256::ZERO).await;
        assert!(result.is_ok(), "expected Ok, got: {}", result.unwrap_err());

        let proof = result.unwrap();

        // Verify 130-byte proof format
        assert_eq!(proof.len(), 130);
        assert_eq!(proof[0], PROOF_TYPE_TEE);
        assert_eq!(&proof[1..33], l1_origin_hash.as_slice());
        assert_eq!(&proof[33..57], &[0u8; 24]);

        let mut number_bytes = [0u8; 8];
        number_bytes.copy_from_slice(&proof[57..65]);
        assert_eq!(u64::from_be_bytes(number_bytes), 500);
        assert_eq!(proof[129], 27);
    }

    // ========================================================================
    // Data transformation helper tests
    // ========================================================================

    #[test]
    fn test_convert_receipts_preserves_log_data() {
        use alloy_consensus::ReceiptEnvelope;

        let log_address = address!("2222222222222222222222222222222222222222");
        let log_data = LogData::new_unchecked(vec![], Bytes::from(vec![0x42]));
        let rpc_log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log { address: log_address, data: log_data.clone() },
            ..Default::default()
        };

        let receipt =
            Receipt { status: true.into(), cumulative_gas_used: 21000, logs: vec![rpc_log] };
        let envelope =
            ReceiptEnvelope::Legacy(ReceiptWithBloom { receipt, logs_bloom: Default::default() });

        let tx_receipt = TransactionReceipt {
            inner: envelope,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: None,
            block_number: None,
            gas_used: 21000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        };

        let converted = ReceiptConverter::convert_receipts(vec![tx_receipt]);
        assert_eq!(converted.len(), 1);

        match &converted[0] {
            ReceiptEnvelope::Legacy(rwb) => {
                assert_eq!(rwb.receipt.logs.len(), 1);
                assert_eq!(rwb.receipt.logs[0].address, log_address);
                assert_eq!(rwb.receipt.logs[0].data, log_data);
            }
            _ => panic!("expected Legacy receipt envelope"),
        }
    }

    #[test]
    fn test_build_chain_config_maps_rollup_fields() {
        let mut rollup = default_rollup_config();
        rollup.batch_inbox_address = address!("1111111111111111111111111111111111111111");
        rollup.protocol_versions_address = address!("2222222222222222222222222222222222222222");
        rollup.deposit_contract_address = address!("3333333333333333333333333333333333333333");
        rollup.l1_system_config_address = address!("4444444444444444444444444444444444444444");
        rollup.block_time = 2;
        rollup.l1_chain_id = 1;

        let config = ConfigBuilder::build_chain_config(&rollup, "base-challenger");

        assert_eq!(config.batch_inbox_addr, rollup.batch_inbox_address);
        assert_eq!(config.block_time, 2);
        assert_eq!(config.l1_chain_id, 1);
        assert_eq!(config.protocol_versions_addr, Some(rollup.protocol_versions_address));

        let addresses = config.addresses.expect("addresses should be populated");
        assert_eq!(addresses.address_manager, Some(rollup.protocol_versions_address));
        assert_eq!(addresses.optimism_portal_proxy, Some(rollup.deposit_contract_address));
        assert_eq!(addresses.system_config_proxy, Some(rollup.l1_system_config_address));
    }
}
