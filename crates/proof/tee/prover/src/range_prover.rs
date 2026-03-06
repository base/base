use std::sync::Arc;

use alloy_primitives::{Address, B256};
use base_enclave::{AggregateRequest, ChainConfig, Proposal, RollupConfig, l2_block_to_block_info};
use base_enclave_client::ExecuteStatelessRequest;
use base_proof_rpc::L1Provider;
use base_protocol::Predeploys;
use tracing::info;

use crate::{
    ConfigBuilder, ENCLAVE_TIMEOUT, ExecutionWitnessProvider, RangeProverError, ReceiptConverter,
    TeeExecutor, TransactionSerializer,
};

/// Proves ranges of L2 blocks in a TEE.
///
/// Shared between the proposer (proving full block intervals) and the
/// challenger (proving intermediate checkpoint intervals).
#[derive(Debug)]
pub struct RangeProver<L1, L2, E> {
    rollup_config: RollupConfig,
    config_hash: B256,
    chain_config: ChainConfig,
    l1_provider: Arc<L1>,
    l2_provider: Arc<L2>,
    enclave_client: Arc<E>,
    proposer: Address,
    tee_image_hash: B256,
}

impl<L1, L2, E> RangeProver<L1, L2, E>
where
    L1: L1Provider,
    L2: ExecutionWitnessProvider,
    E: TeeExecutor,
{
    /// Creates a new range prover.
    ///
    /// Computes `config_hash` and `chain_config` from the rollup configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the config hash cannot be computed (e.g. missing
    /// genesis `system_config`).
    pub fn new(
        rollup_config: RollupConfig,
        l1_provider: Arc<L1>,
        l2_provider: Arc<L2>,
        enclave_client: Arc<E>,
        proposer: Address,
        tee_image_hash: B256,
        name: &str,
    ) -> Result<Self, RangeProverError> {
        let config_hash = ConfigBuilder::compute_config_hash(&rollup_config)
            .map_err(|e| RangeProverError::Config(e.to_string()))?;
        let chain_config = ConfigBuilder::build_chain_config(&rollup_config, name);

        Ok(Self {
            rollup_config,
            config_hash,
            chain_config,
            l1_provider,
            l2_provider,
            enclave_client,
            proposer,
            tee_image_hash,
        })
    }

    /// Returns the configuration hash.
    pub const fn config_hash(&self) -> B256 {
        self.config_hash
    }

    /// Proves a single L2 block inside the TEE.
    ///
    /// Fetches the block, derives L1 origin, fetches data in parallel
    /// (witness, proofs, L1 origin header, L1 receipts), serializes
    /// transactions, and calls `execute_stateless` on the enclave.
    pub async fn prove_block(&self, block_number: u64) -> Result<Proposal, RangeProverError> {
        let block = self
            .l2_provider
            .block_by_number(Some(block_number))
            .await
            .map_err(|e| RangeProverError::Rpc { context: "target block", source: e })?;

        let block_hash = block.header.hash;

        // Get the first transaction to derive L1 origin info
        let first_tx = block
            .transactions
            .txns()
            .next()
            .ok_or_else(|| RangeProverError::DataPrep("no transactions in block".into()))?;

        let first_tx_bytes = TransactionSerializer::serialize_rpc_transaction(first_tx)
            .map_err(|e| RangeProverError::DataPrep(e.to_string()))?;

        // Derive L2 block info to get L1 origin
        let l2_block_info = l2_block_to_block_info(
            &self.rollup_config,
            &block.header.inner,
            block_hash,
            &first_tx_bytes,
        )
        .map_err(|e| RangeProverError::DataPrep(format!("L2 block info derivation: {e}")))?;

        let l1_origin_hash = l2_block_info.l1_origin.hash;

        // Fetch all required data in parallel
        let (
            witness_result,
            msg_account_result,
            prev_block_result,
            prev_msg_account_result,
            l1_origin_result,
            l1_receipts_result,
        ) = tokio::join!(
            self.l2_provider.execution_witness(block_number),
            self.l2_provider.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, block_hash),
            self.l2_provider.block_by_hash(block.header.parent_hash),
            self.l2_provider
                .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, block.header.parent_hash),
            self.l1_provider.header_by_hash(l1_origin_hash),
            self.l1_provider.block_receipts(l1_origin_hash),
        );

        let witness = witness_result
            .map_err(|e| RangeProverError::Rpc { context: "execution witness", source: e })?;
        let msg_account = msg_account_result
            .map_err(|e| RangeProverError::Rpc { context: "message account proof", source: e })?;
        let prev_block = prev_block_result
            .map_err(|e| RangeProverError::Rpc { context: "previous block", source: e })?;
        let prev_msg_account = prev_msg_account_result.map_err(|e| RangeProverError::Rpc {
            context: "previous message account",
            source: e,
        })?;
        let l1_origin = l1_origin_result
            .map_err(|e| RangeProverError::Rpc { context: "L1 origin header", source: e })?;
        let l1_receipts = l1_receipts_result
            .map_err(|e| RangeProverError::Rpc { context: "L1 receipts", source: e })?;

        // Serialize previous block transactions (all types including deposits)
        let prev_block_txs = TransactionSerializer::serialize_block_transactions(&prev_block, true)
            .map_err(|e| RangeProverError::DataPrep(e.to_string()))?;

        // Serialize current block transactions (excluding deposits)
        let sequenced_txs = TransactionSerializer::serialize_block_transactions(&block, false)
            .map_err(|e| RangeProverError::DataPrep(e.to_string()))?;

        let l1_receipt_envelopes = ReceiptConverter::convert_receipts(l1_receipts);

        let request = ExecuteStatelessRequest {
            config: self.chain_config.clone(),
            config_hash: self.config_hash,
            l1_origin: l1_origin.inner,
            l1_receipts: l1_receipt_envelopes,
            previous_block_txs: prev_block_txs,
            block_header: block.header.inner,
            sequenced_txs,
            witness,
            message_account: msg_account,
            prev_message_account_hash: prev_msg_account.storage_hash,
            proposer: self.proposer,
            tee_image_hash: self.tee_image_hash,
        };

        tokio::time::timeout(ENCLAVE_TIMEOUT, self.enclave_client.execute_stateless(request))
            .await
            .map_err(|_| RangeProverError::Timeout)?
            .map_err(RangeProverError::Enclave)
    }

    /// Aggregates multiple proposals into a single proposal.
    ///
    /// A single proposal is returned as-is. Multiple proposals are aggregated
    /// via the enclave.
    pub async fn aggregate(
        &self,
        prev_output_root: B256,
        prev_block_number: u64,
        proposals: Vec<Proposal>,
        intermediate_roots: Vec<B256>,
    ) -> Result<Proposal, RangeProverError> {
        if proposals.is_empty() {
            return Err(RangeProverError::Empty);
        }
        if proposals.len() == 1 {
            return Ok(proposals.into_iter().next().unwrap());
        }

        let request = AggregateRequest {
            config_hash: self.config_hash,
            prev_output_root,
            prev_block_number,
            proposals,
            proposer: self.proposer,
            tee_image_hash: self.tee_image_hash,
            intermediate_roots,
        };

        tokio::time::timeout(ENCLAVE_TIMEOUT, self.enclave_client.aggregate(request))
            .await
            .map_err(|_| RangeProverError::Timeout)?
            .map_err(RangeProverError::Enclave)
    }

    /// Proves a contiguous range of L2 blocks and aggregates the results.
    ///
    /// Executes `prove_block` for each block in `(prev_block_number, prev_block_number + block_count]`,
    /// then aggregates all proposals.
    pub async fn prove_range(
        &self,
        prev_block_number: u64,
        block_count: u64,
        prev_output_root: B256,
        intermediate_roots: Vec<B256>,
    ) -> Result<Proposal, RangeProverError> {
        if block_count == 0 {
            return Err(RangeProverError::Empty);
        }

        let end_block = prev_block_number.checked_add(block_count).ok_or_else(|| {
            RangeProverError::DataPrep("arithmetic overflow computing end block".into())
        })?;

        let mut proposals = Vec::with_capacity(block_count as usize);
        for block_number in (prev_block_number + 1)..=end_block {
            info!(block = %block_number, "proving block");
            let proposal = self.prove_block(block_number).await?;
            proposals.push(proposal);
        }

        self.aggregate(prev_output_root, prev_block_number, proposals, intermediate_roots).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_consensus::Header as ConsensusHeader;
    use alloy_primitives::{Bytes, U256};
    use async_trait::async_trait;
    use base_alloy_consensus::{OpTxEnvelope, TxDeposit};
    use base_enclave::{AccountResult, ExecutionWitness, Proposal, default_rollup_config};
    use base_enclave_client::ClientError;
    use base_proof_rpc::{L2Provider, OpBlock, RpcResult};
    use base_protocol::L1BlockInfoBedrock;

    use super::*;
    use crate::ExecutionWitnessProvider;

    // ========================================================================
    // Mock types
    // ========================================================================

    struct MockEnclave {
        result: Result<Proposal, ClientError>,
        aggregate_result: Result<Proposal, ClientError>,
    }

    #[async_trait]
    impl TeeExecutor for MockEnclave {
        async fn execute_stateless(
            &self,
            _req: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            match &self.result {
                Ok(p) => Ok(p.clone()),
                Err(e) => Err(ClientError::ClientCreation(e.to_string())),
            }
        }

        async fn aggregate(&self, _req: AggregateRequest) -> Result<Proposal, ClientError> {
            match &self.aggregate_result {
                Ok(p) => Ok(p.clone()),
                Err(e) => Err(ClientError::ClientCreation(e.to_string())),
            }
        }
    }

    #[derive(Debug)]
    struct MockL1 {
        headers: HashMap<B256, alloy_rpc_types_eth::Header>,
        receipts: HashMap<B256, Vec<alloy_rpc_types_eth::TransactionReceipt>>,
    }

    #[async_trait]
    impl L1Provider for MockL1 {
        async fn block_number(&self) -> RpcResult<u64> {
            Ok(0)
        }
        async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
            unimplemented!()
        }
        async fn header_by_hash(&self, hash: B256) -> RpcResult<alloy_rpc_types_eth::Header> {
            self.headers.get(&hash).cloned().ok_or_else(|| {
                base_proof_rpc::RpcError::HeaderNotFound(format!("no header for {hash}"))
            })
        }
        async fn block_receipts(
            &self,
            hash: B256,
        ) -> RpcResult<Vec<alloy_rpc_types_eth::TransactionReceipt>> {
            self.receipts.get(&hash).cloned().ok_or_else(|| {
                base_proof_rpc::RpcError::BlockNotFound(format!("no receipts for {hash}"))
            })
        }
        async fn code_at(&self, _: Address, _: Option<u64>) -> RpcResult<Bytes> {
            Ok(Bytes::new())
        }
        async fn call_contract(&self, _: Address, _: Bytes, _: Option<u64>) -> RpcResult<Bytes> {
            Ok(Bytes::new())
        }
        async fn get_balance(&self, _: Address) -> RpcResult<U256> {
            Ok(U256::ZERO)
        }
    }

    #[derive(Debug)]
    struct MockL2 {
        blocks: HashMap<u64, OpBlock>,
        blocks_by_hash: HashMap<B256, OpBlock>,
        proofs: HashMap<B256, AccountResult>,
        witness: Option<ExecutionWitness>,
    }

    impl MockL2 {
        fn new() -> Self {
            Self {
                blocks: HashMap::new(),
                blocks_by_hash: HashMap::new(),
                proofs: HashMap::new(),
                witness: None,
            }
        }
    }

    #[async_trait]
    impl L2Provider for MockL2 {
        async fn chain_config(&self) -> RpcResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
        async fn get_proof(&self, _: Address, hash: B256) -> RpcResult<AccountResult> {
            self.proofs.get(&hash).cloned().ok_or_else(|| {
                base_proof_rpc::RpcError::ProofNotFound(format!("no proof for {hash}"))
            })
        }
        async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
            unimplemented!()
        }
        async fn block_by_number(&self, number: Option<u64>) -> RpcResult<OpBlock> {
            let n = number.unwrap_or(0);
            self.blocks
                .get(&n)
                .cloned()
                .ok_or_else(|| base_proof_rpc::RpcError::BlockNotFound(format!("block {n}")))
        }
        async fn block_by_hash(&self, hash: B256) -> RpcResult<OpBlock> {
            self.blocks_by_hash
                .get(&hash)
                .cloned()
                .ok_or_else(|| base_proof_rpc::RpcError::BlockNotFound(format!("block {hash}")))
        }
    }

    #[async_trait]
    impl ExecutionWitnessProvider for MockL2 {
        async fn execution_witness(&self, _: u64) -> RpcResult<ExecutionWitness> {
            self.witness
                .clone()
                .ok_or_else(|| base_proof_rpc::RpcError::BlockNotFound("no witness".into()))
        }
    }

    fn zero_proposal() -> Proposal {
        Proposal {
            output_root: B256::ZERO,
            signature: Bytes::new(),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: U256::ZERO,
            l2_block_number: U256::ZERO,
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        }
    }

    fn make_deposit_tx(l1_hash: B256, l1_number: u64) -> base_alloy_rpc_types::Transaction {
        use alloy_consensus::Sealable;
        use alloy_primitives::TxKind;

        let l1_info = L1BlockInfoBedrock::new(
            l1_number,
            1_700_000_000,
            1_000_000_000,
            l1_hash,
            0,
            Address::ZERO,
            U256::ZERO,
            U256::from(684000),
        );
        let calldata = base_protocol::L1BlockInfoTx::Bedrock(l1_info).encode_calldata();

        let deposit = TxDeposit {
            source_hash: B256::repeat_byte(0x01),
            from: alloy_primitives::address!("DeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001"),
            to: TxKind::Call(alloy_primitives::address!(
                "4200000000000000000000000000000000000015"
            )),
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

    fn make_test_block(
        block_number: u64,
        parent_hash: B256,
        l1_origin_hash: B256,
        l1_origin_number: u64,
    ) -> OpBlock {
        use alloy_rpc_types_eth::{Block, BlockTransactions, Header as RpcHeader};

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

    fn mock_account_result() -> AccountResult {
        AccountResult {
            address: Predeploys::L2_TO_L1_MESSAGE_PASSER,
            account_proof: vec![],
            balance: U256::ZERO,
            code_hash: alloy_primitives::b256!(
                "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
            ),
            nonce: U256::ZERO,
            storage_hash: B256::repeat_byte(0x01),
            storage_proof: vec![],
        }
    }

    fn wired_providers(target_block_number: u64) -> (MockL2, MockL1) {
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

        let l1_header = alloy_rpc_types_eth::Header {
            hash: l1_origin_hash,
            inner: ConsensusHeader { number: l1_origin_number, ..Default::default() },
            ..Default::default()
        };

        let mut l2 = MockL2::new();
        l2.blocks.insert(target_block_number, target_block);
        l2.blocks_by_hash.insert(parent_hash, prev_block);
        l2.proofs.insert(target_hash, account.clone());
        l2.proofs.insert(parent_hash, account);
        l2.witness = Some(witness);

        let mut l1_headers = HashMap::new();
        l1_headers.insert(l1_origin_hash, l1_header);
        let mut l1_receipts = HashMap::new();
        l1_receipts.insert(l1_origin_hash, vec![]);

        let l1 = MockL1 { headers: l1_headers, receipts: l1_receipts };
        (l2, l1)
    }

    fn test_range_prover(
        l2: MockL2,
        l1: MockL1,
        enclave: MockEnclave,
    ) -> RangeProver<MockL1, MockL2, MockEnclave> {
        RangeProver::new(
            default_rollup_config(),
            Arc::new(l1),
            Arc::new(l2),
            Arc::new(enclave),
            Address::ZERO,
            B256::ZERO,
            "test",
        )
        .expect("config hash should compute")
    }

    // ========================================================================
    // prove_block tests
    // ========================================================================

    #[tokio::test]
    async fn test_prove_block_happy_path() {
        let target = 110u64;
        let (l2, l1) = wired_providers(target);
        let proposal = zero_proposal();
        let enclave =
            MockEnclave { result: Ok(proposal.clone()), aggregate_result: Ok(zero_proposal()) };
        let prover = test_range_prover(l2, l1, enclave);

        let result = prover.prove_block(target).await;
        assert!(result.is_ok(), "expected Ok, got: {}", result.unwrap_err());
    }

    #[tokio::test]
    async fn test_prove_block_missing_block() {
        let l2 = MockL2::new();
        let l1 = MockL1 { headers: HashMap::new(), receipts: HashMap::new() };
        let enclave =
            MockEnclave { result: Ok(zero_proposal()), aggregate_result: Ok(zero_proposal()) };
        let prover = test_range_prover(l2, l1, enclave);

        let result = prover.prove_block(999).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RangeProverError::Rpc { context: "target block", .. }
        ));
    }

    #[tokio::test]
    async fn test_prove_block_enclave_error() {
        let target = 110u64;
        let (l2, l1) = wired_providers(target);
        let enclave = MockEnclave {
            result: Err(ClientError::ClientCreation("enclave down".into())),
            aggregate_result: Ok(zero_proposal()),
        };
        let prover = test_range_prover(l2, l1, enclave);

        let result = prover.prove_block(target).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RangeProverError::Enclave(_)));
    }

    // ========================================================================
    // aggregate tests
    // ========================================================================

    #[tokio::test]
    async fn test_aggregate_empty() {
        let l2 = MockL2::new();
        let l1 = MockL1 { headers: HashMap::new(), receipts: HashMap::new() };
        let enclave =
            MockEnclave { result: Ok(zero_proposal()), aggregate_result: Ok(zero_proposal()) };
        let prover = test_range_prover(l2, l1, enclave);

        let result = prover.aggregate(B256::ZERO, 0, vec![], vec![]).await;
        assert!(matches!(result.unwrap_err(), RangeProverError::Empty));
    }

    #[tokio::test]
    async fn test_aggregate_single() {
        let l2 = MockL2::new();
        let l1 = MockL1 { headers: HashMap::new(), receipts: HashMap::new() };
        let proposal = Proposal {
            output_root: B256::repeat_byte(0xAA),
            signature: Bytes::from(vec![0xBB; 65]),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: U256::ZERO,
            l2_block_number: U256::from(5),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        };
        let enclave =
            MockEnclave { result: Ok(zero_proposal()), aggregate_result: Ok(zero_proposal()) };
        let prover = test_range_prover(l2, l1, enclave);

        let result = prover.aggregate(B256::ZERO, 0, vec![proposal.clone()], vec![]).await.unwrap();
        assert_eq!(result.output_root, proposal.output_root);
    }

    #[tokio::test]
    async fn test_aggregate_multiple() {
        let l2 = MockL2::new();
        let l1 = MockL1 { headers: HashMap::new(), receipts: HashMap::new() };
        let agg_output = Proposal {
            output_root: B256::repeat_byte(0xCC),
            signature: Bytes::from(vec![0xDD; 65]),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: U256::ZERO,
            l2_block_number: U256::from(10),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        };
        let enclave =
            MockEnclave { result: Ok(zero_proposal()), aggregate_result: Ok(agg_output.clone()) };
        let prover = test_range_prover(l2, l1, enclave);

        let p1 = zero_proposal();
        let p2 = zero_proposal();
        let result = prover.aggregate(B256::ZERO, 0, vec![p1, p2], vec![]).await.unwrap();
        assert_eq!(result.output_root, agg_output.output_root);
    }

    // ========================================================================
    // prove_range tests
    // ========================================================================

    #[tokio::test]
    async fn test_prove_range_empty() {
        let l2 = MockL2::new();
        let l1 = MockL1 { headers: HashMap::new(), receipts: HashMap::new() };
        let enclave =
            MockEnclave { result: Ok(zero_proposal()), aggregate_result: Ok(zero_proposal()) };
        let prover = test_range_prover(l2, l1, enclave);

        let result = prover.prove_range(100, 0, B256::ZERO, vec![]).await;
        assert!(matches!(result.unwrap_err(), RangeProverError::Empty));
    }

    #[tokio::test]
    async fn test_prove_range_overflow() {
        let l2 = MockL2::new();
        let l1 = MockL1 { headers: HashMap::new(), receipts: HashMap::new() };
        let enclave =
            MockEnclave { result: Ok(zero_proposal()), aggregate_result: Ok(zero_proposal()) };
        let prover = test_range_prover(l2, l1, enclave);

        let result = prover.prove_range(u64::MAX - 5, 10, B256::ZERO, vec![]).await;
        assert!(matches!(result.unwrap_err(), RangeProverError::DataPrep(_)));
    }

    #[tokio::test]
    async fn test_prove_range_single_block() {
        let target = 110u64;
        let (l2, l1) = wired_providers(target);
        let proposal = Proposal {
            output_root: B256::repeat_byte(0xFF),
            signature: Bytes::from(vec![0xAA; 65]),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: U256::ZERO,
            l2_block_number: U256::from(target),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        };
        let enclave =
            MockEnclave { result: Ok(proposal.clone()), aggregate_result: Ok(zero_proposal()) };
        let prover = test_range_prover(l2, l1, enclave);

        // Single block range: prev=109, count=1 => proves block 110
        let result = prover.prove_range(109, 1, B256::ZERO, vec![]).await.unwrap();
        assert_eq!(result.output_root, proposal.output_root);
    }

    // ========================================================================
    // is_retryable tests
    // ========================================================================

    #[test]
    fn test_range_prover_error_retryable_rpc() {
        let err = RangeProverError::Rpc {
            context: "target block",
            source: base_proof_rpc::RpcError::Transport("connection reset".into()),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_range_prover_error_not_retryable_empty() {
        assert!(!RangeProverError::Empty.is_retryable());
    }

    #[test]
    fn test_range_prover_error_not_retryable_timeout() {
        assert!(!RangeProverError::Timeout.is_retryable());
    }
}
