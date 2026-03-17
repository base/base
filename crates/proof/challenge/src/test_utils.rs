//! Test utilities: mock stubs for contract clients, ZK proof provider, tx manager, and scanner
//! tests.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use alloy_consensus::{
    Eip658Value, Header as ConsensusHeader, Receipt, ReceiptEnvelope, ReceiptWithBloom,
};
use alloy_primitives::{Address, B256, Bloom, Bytes, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{Header as RpcHeader, TransactionReceipt};
use alloy_trie::{HashBuilder, Nibbles, proof::ProofRetainer};
use async_trait::async_trait;
use base_enclave::AccountResult;
use base_proof_contracts::{
    AggregateVerifierClient, ContractError, DisputeGameFactoryClient, GameAtIndex, GameInfo,
};
use base_proof_rpc::{L2Provider, RpcError, RpcResult};
use base_protocol::Predeploys;
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};
use base_zk_client::{
    GetProofRequest, GetProofResponse, ProveBlockRequest, ProveBlockResponse, ZkProofError,
    ZkProofProvider,
};

/// Per-game state for the mock verifier.
#[derive(Debug, Clone)]
pub struct MockGameState {
    /// Game status (`0=IN_PROGRESS`, `1=CHALLENGER_WINS`, `2=DEFENDER_WINS`).
    pub status: u8,
    /// Address of the ZK prover (`Address::ZERO` if unchallenged).
    pub zk_prover: Address,
    /// Address of the TEE prover (`Address::ZERO` if no TEE proof submitted).
    pub tee_prover: Address,
    /// Game info (root claim, L2 block number, parent index).
    pub game_info: GameInfo,
    /// Starting block number for this game.
    pub starting_block_number: u64,
    /// Intermediate output roots for this game.
    pub intermediate_output_roots: Vec<B256>,
}

/// Mock dispute game factory with configurable per-index game data.
#[derive(Debug)]
pub struct MockDisputeGameFactory {
    /// Ordered list of games in the factory.
    pub games: Vec<GameAtIndex>,
}

#[async_trait]
impl DisputeGameFactoryClient for MockDisputeGameFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        Ok(self.games.len() as u64)
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        self.games
            .get(index as usize)
            .cloned()
            .ok_or_else(|| ContractError::Validation(format!("index {index} out of bounds")))
    }

    async fn init_bonds(&self, _game_type: u32) -> Result<U256, ContractError> {
        Ok(U256::ZERO)
    }

    async fn game_impls(&self, _game_type: u32) -> Result<Address, ContractError> {
        Ok(Address::repeat_byte(0x11))
    }
}

/// Mock aggregate verifier with configurable per-address game state.
#[derive(Debug)]
pub struct MockAggregateVerifier {
    /// Per-address game state lookup.
    pub games: HashMap<Address, MockGameState>,
}

#[async_trait]
impl AggregateVerifierClient for MockAggregateVerifier {
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.game_info.clone())
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn status(&self, game_address: Address) -> Result<u8, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.status)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn zk_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.zk_prover)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn tee_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.tee_prover)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn starting_block_number(&self, game_address: Address) -> Result<u64, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.starting_block_number)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn read_block_interval(&self, _impl_address: Address) -> Result<u64, ContractError> {
        Ok(10)
    }

    async fn read_intermediate_block_interval(
        &self,
        _impl_address: Address,
    ) -> Result<u64, ContractError> {
        Ok(5)
    }

    async fn intermediate_output_roots(
        &self,
        game_address: Address,
    ) -> Result<Vec<B256>, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.intermediate_output_roots.clone())
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }
}

/// Helper to create an address from a `u64` index.
pub fn addr(index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..20].copy_from_slice(&index.to_be_bytes());
    Address::from(bytes)
}

/// Helper to build a factory game entry.
pub fn factory_game(index: u64, game_type: u32) -> GameAtIndex {
    GameAtIndex { game_type, timestamp: 1_000_000 + index, proxy: addr(index) }
}

/// Helper to build mock game state for the verifier.
pub const fn mock_state(status: u8, zk_prover: Address, block_number: u64) -> MockGameState {
    mock_state_with_tee(status, zk_prover, Address::ZERO, block_number)
}

/// Helper to build mock game state with an explicit TEE prover address.
pub const fn mock_state_with_tee(
    status: u8,
    zk_prover: Address,
    tee_prover: Address,
    block_number: u64,
) -> MockGameState {
    MockGameState {
        status,
        zk_prover,
        tee_prover,
        game_info: GameInfo {
            root_claim: B256::repeat_byte(block_number as u8),
            l2_block_number: block_number,
            parent_index: 0,
        },
        starting_block_number: block_number.saturating_sub(10),
        intermediate_output_roots: vec![],
    }
}

/// Mock factory that returns an error for specific indices.
#[derive(Debug)]
pub struct ErrorOnIndexFactory {
    /// The inner factory providing normal game data.
    pub inner: MockDisputeGameFactory,
    /// Indices that should return an error when queried.
    pub error_indices: Vec<u64>,
}

#[async_trait]
impl DisputeGameFactoryClient for ErrorOnIndexFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        self.inner.game_count().await
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        if self.error_indices.contains(&index) {
            return Err(ContractError::Validation(format!("simulated error at index {index}")));
        }
        self.inner.game_at_index(index).await
    }

    async fn init_bonds(&self, game_type: u32) -> Result<U256, ContractError> {
        self.inner.init_bonds(game_type).await
    }

    async fn game_impls(&self, game_type: u32) -> Result<Address, ContractError> {
        self.inner.game_impls(game_type).await
    }
}

/// Mock L2 provider with configurable block headers and storage proofs.
///
/// Returns pre-configured headers by block number and account proofs by
/// block hash. Block numbers in `error_blocks` will return a
/// [`RpcError::BlockNotFound`] to simulate missing blocks.
#[derive(Debug)]
pub struct MockL2Provider {
    /// Headers keyed by block number.
    pub headers: HashMap<u64, RpcHeader>,
    /// Account proofs keyed by block hash.
    pub proofs: HashMap<B256, AccountResult>,
    /// Block numbers that should return an error (simulating missing blocks).
    pub error_blocks: Vec<u64>,
}

impl MockL2Provider {
    /// Creates a new empty mock L2 provider.
    pub fn new() -> Self {
        Self { headers: HashMap::new(), proofs: HashMap::new(), error_blocks: Vec::new() }
    }

    /// Inserts a block header and corresponding account proof.
    ///
    /// The consensus header is wrapped in an RPC header with the hash computed
    /// from [`ConsensusHeader::hash_slow`].
    pub fn insert_block(
        &mut self,
        block_number: u64,
        consensus_header: ConsensusHeader,
        account_result: AccountResult,
    ) {
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };
        self.headers.insert(block_number, rpc_header);
        self.proofs.insert(block_hash, account_result);
    }
}

impl Default for MockL2Provider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl L2Provider for MockL2Provider {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }

    async fn get_proof(&self, _address: Address, block_hash: B256) -> RpcResult<AccountResult> {
        self.proofs
            .get(&block_hash)
            .cloned()
            .ok_or_else(|| RpcError::ProofNotFound(format!("no proof for hash {block_hash}")))
    }

    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<RpcHeader> {
        let block_number = number.unwrap_or(0);
        if self.error_blocks.contains(&block_number) {
            return Err(RpcError::BlockNotFound(format!("block {block_number} not available")));
        }
        self.headers
            .get(&block_number)
            .cloned()
            .ok_or_else(|| RpcError::HeaderNotFound(format!("no header for block {block_number}")))
    }

    async fn block_by_number(&self, _number: Option<u64>) -> RpcResult<base_proof_rpc::OpBlock> {
        Err(RpcError::BlockNotFound("not implemented in mock".into()))
    }

    async fn block_by_hash(&self, _hash: B256) -> RpcResult<base_proof_rpc::OpBlock> {
        Err(RpcError::BlockNotFound("not implemented in mock".into()))
    }
}

/// Mock ZK proof provider for testing the driver.
#[derive(Debug)]
pub struct MockZkProofProvider {
    /// Session ID returned by [`prove_block`](ZkProofProvider::prove_block).
    pub session_id: String,
    /// Proof job status returned by [`get_proof`](ZkProofProvider::get_proof).
    pub proof_status: Mutex<i32>,
    /// Proof receipt bytes returned when status is `Succeeded`.
    pub receipt: Mutex<Vec<u8>>,
}

#[async_trait]
impl ZkProofProvider for MockZkProofProvider {
    async fn prove_block(
        &self,
        _request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        Ok(ProveBlockResponse { session_id: self.session_id.clone() })
    }

    async fn get_proof(&self, _request: GetProofRequest) -> Result<GetProofResponse, ZkProofError> {
        let status = *self.proof_status.lock().unwrap();
        let receipt = self.receipt.lock().unwrap().clone();
        Ok(GetProofResponse { status, receipt })
    }
}

/// Mock transaction manager for testing the driver and submitter.
#[derive(Debug)]
pub struct MockTxManager {
    /// Queue of responses returned by [`send`](TxManager::send).
    pub responses: Mutex<VecDeque<SendResponse>>,
}

impl MockTxManager {
    /// Creates a new mock with a single pre-configured response.
    pub fn new(response: SendResponse) -> Self {
        let mut q = VecDeque::new();
        q.push_back(response);
        Self { responses: Mutex::new(q) }
    }

    /// Creates a new mock with multiple responses returned in order.
    pub fn with_responses(responses: Vec<SendResponse>) -> Self {
        Self { responses: Mutex::new(VecDeque::from(responses)) }
    }
}

impl TxManager for MockTxManager {
    async fn send(&self, _candidate: TxCandidate) -> SendResponse {
        self.responses.lock().unwrap().pop_front().expect("MockTxManager has no more responses")
    }

    async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
        unimplemented!("not needed for these tests")
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}

/// Builds a minimal [`TransactionReceipt`] with the given status and hash.
pub const fn receipt_with_status(success: bool, tx_hash: B256) -> TransactionReceipt {
    let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
        receipt: Receipt {
            status: Eip658Value::Eip658(success),
            cumulative_gas_used: 21_000,
            logs: vec![],
        },
        logs_bloom: Bloom::ZERO,
    });
    TransactionReceipt {
        inner,
        transaction_hash: tx_hash,
        transaction_index: Some(0),
        block_hash: Some(B256::ZERO),
        block_number: Some(1),
        gas_used: 21_000,
        effective_gas_price: 1_000_000_000,
        blob_gas_used: None,
        blob_gas_price: None,
        from: Address::ZERO,
        to: Some(Address::ZERO),
        contract_address: None,
    }
}

/// Account structure for RLP encoding in tests.
#[derive(Debug)]
pub struct TrieAccount {
    /// Account nonce.
    pub nonce: u64,
    /// Account balance.
    pub balance: U256,
    /// Storage root hash.
    pub storage_root: B256,
    /// Code hash.
    pub code_hash: B256,
}

impl Encodable for TrieAccount {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let header = alloy_rlp::Header {
            list: true,
            payload_length: self.nonce.length()
                + self.balance.length()
                + self.storage_root.length()
                + self.code_hash.length(),
        };
        header.encode(out);
        self.nonce.encode(out);
        self.balance.encode(out);
        self.storage_root.encode(out);
        self.code_hash.encode(out);
    }

    fn length(&self) -> usize {
        let payload_length = self.nonce.length()
            + self.balance.length()
            + self.storage_root.length()
            + self.code_hash.length();
        alloy_rlp::length_of_length(payload_length) + payload_length
    }
}

/// Builds a consensus header and account result pair with a valid Merkle
/// proof. The returned header's `state_root` is the trie root that the
/// account proof verifies against.
pub fn build_test_header_and_account(
    block_number: u64,
    storage_hash: B256,
) -> (ConsensusHeader, AccountResult) {
    let account = TrieAccount {
        nonce: 0,
        balance: U256::ZERO,
        storage_root: storage_hash,
        code_hash: B256::ZERO,
    };
    let mut encoded = Vec::with_capacity(account.length());
    account.encode(&mut encoded);

    let account_key = Nibbles::unpack(keccak256(Predeploys::L2_TO_L1_MESSAGE_PASSER));
    let mut hb = HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![account_key]));
    hb.add_leaf(account_key, &encoded);
    let state_root = hb.root();
    let proof_nodes = hb.take_proof_nodes();
    let account_proof: Vec<Bytes> =
        proof_nodes.into_nodes_sorted().into_iter().map(|(_, v)| v).collect();

    let header = ConsensusHeader { number: block_number, state_root, ..Default::default() };
    let account_result = AccountResult {
        address: Predeploys::L2_TO_L1_MESSAGE_PASSER,
        account_proof,
        balance: U256::ZERO,
        code_hash: B256::ZERO,
        nonce: U256::ZERO,
        storage_hash,
        storage_proof: vec![],
    };
    (header, account_result)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::scanner::{GameScanner, ScannerConfig};

    /// Happy path: mixed games, only `IN_PROGRESS` / unchallenged returned.
    #[tokio::test]
    async fn test_scan_happy_path() {
        // Game 0: type 1, IN_PROGRESS, unchallenged -> candidate
        // Game 1: type 99, IN_PROGRESS, unchallenged -> candidate (all types scanned)
        // Game 2: type 1, status=1 (not in progress) -> skipped
        // Game 3: type 1, IN_PROGRESS, already challenged -> skipped
        // Game 4: type 1, IN_PROGRESS, unchallenged -> candidate
        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![
                factory_game(0, 1),
                factory_game(1, 99),
                factory_game(2, 1),
                factory_game(3, 1),
                factory_game(4, 1),
            ],
        });

        let challenger_addr = Address::repeat_byte(0xCC);
        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 150));
        verifier_games.insert(addr(2), mock_state(1, Address::ZERO, 200));
        verifier_games.insert(addr(3), mock_state(0, challenger_addr, 300));
        verifier_games.insert(addr(4), mock_state(0, Address::ZERO, 400));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        // last_scanned=None, start = max(0, 5-1000) = 0, so games 0..=4 scanned
        // Game 0: candidate. Game 1: candidate. Game 2: status != 0.
        // Game 3: challenged. Game 4: candidate.
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[0].factory.game_type, 1);
        assert_eq!(candidates[0].info.l2_block_number, 100);
        assert_eq!(candidates[1].index, 1);
        assert_eq!(candidates[1].factory.game_type, 99);
        assert_eq!(candidates[1].info.l2_block_number, 150);
        assert_eq!(candidates[2].index, 4);
        assert_eq!(candidates[2].factory.game_type, 1);
        assert_eq!(candidates[2].info.l2_block_number, 400);
        assert_eq!(new_last_scanned, Some(4));
    }

    /// Already-challenged games (zkProver != zero) are filtered out.
    #[tokio::test]
    async fn test_scan_filters_challenged_games() {
        let challenger_addr = Address::repeat_byte(0xAA);

        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1)],
        });

        let mut verifier_games = HashMap::new();
        // All IN_PROGRESS but index 0 and 2 are already challenged
        verifier_games.insert(addr(0), mock_state(0, challenger_addr, 100));
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));
        verifier_games.insert(addr(2), mock_state(0, challenger_addr, 300));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        // Scan from the beginning (last_scanned=None, lookback covers all)
        // start = max(0, 3-1000) = 0, end = 2
        // Game 0: challenged -> skip. Game 1: unchallenged -> candidate. Game 2: challenged -> skip.
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
        assert_eq!(new_last_scanned, Some(2));
    }

    /// Empty factory returns empty vec without error.
    #[tokio::test]
    async fn test_scan_empty_factory() {
        let factory = Arc::new(MockDisputeGameFactory { games: vec![] });
        let verifier = Arc::new(MockAggregateVerifier { games: HashMap::new() });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert!(candidates.is_empty());
        assert_eq!(new_last_scanned, None);
    }

    /// No new games since last scan returns empty vec.
    #[tokio::test]
    async fn test_scan_no_new_games() {
        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 1), factory_game(1, 1)],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        // last_scanned = Some(1) (gameCount - 1), so start = 2 > end = 1
        let (candidates, new_last_scanned) = scanner.scan(Some(1)).await.unwrap();

        assert!(candidates.is_empty());
        assert_eq!(new_last_scanned, Some(1));
    }

    /// Lookback window: on fresh start with large factory, only `lookback_games` are scanned.
    #[tokio::test]
    async fn test_scan_lookback_window() {
        // Factory with 100 games, but lookback is 3 -> only scan indices 97, 98, 99
        let mut games = Vec::new();
        let mut verifier_games = HashMap::new();

        for i in 0..100u64 {
            games.push(factory_game(i, 1));
            verifier_games.insert(addr(i), mock_state(0, Address::ZERO, i * 10));
        }

        let factory = Arc::new(MockDisputeGameFactory { games });
        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 3 });

        // Fresh start: last_scanned = None
        // start = max(0, 100-3) = 97, end = 99
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].index, 97);
        assert_eq!(candidates[1].index, 98);
        assert_eq!(candidates[2].index, 99);
        assert_eq!(new_last_scanned, Some(99));
    }

    /// Error resilience: a per-game error is logged and skipped, other games still returned.
    /// `new_last_scanned` is set to one before the errored index so it will be retried.
    #[tokio::test]
    async fn test_scan_skips_errored_games() {
        // 3 games: index 1 will error, indices 0 and 2 are valid candidates
        let factory = Arc::new(ErrorOnIndexFactory {
            inner: MockDisputeGameFactory {
                games: vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1)],
            },
            error_indices: vec![1],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        // index 1 won't be queried on the verifier because the factory errors first
        verifier_games.insert(addr(2), mock_state(0, Address::ZERO, 300));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        // start = max(0, 3-1000) = 0, end = 2
        // Index 0 -> candidate. Index 1 errors -> skipped. Index 2 -> candidate.
        // new_last_scanned = lowest_error(1) - 1 = 0, so next scan retries from index 1.
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[1].index, 2);
        assert_eq!(new_last_scanned, Some(0));
    }

    /// Retry: errored games are retried on the next scan when the error clears.
    #[tokio::test]
    async fn test_scan_retries_errored_games() {
        // Phase 1: index 1 errors, so new_last_scanned = Some(0)
        let factory = Arc::new(ErrorOnIndexFactory {
            inner: MockDisputeGameFactory {
                games: vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1)],
            },
            error_indices: vec![1],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));
        verifier_games.insert(addr(2), mock_state(0, Address::ZERO, 300));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games.clone() });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let (_, new_last_scanned) = scanner.scan(None).await.unwrap();
        assert_eq!(new_last_scanned, Some(0));

        // Phase 2: no errors, pass last_scanned = Some(0) to retry from index 1
        let factory2 = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1)],
        });

        let verifier2 = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner2 =
            GameScanner::new(factory2, verifier2, ScannerConfig { lookback_games: 1000 });

        let (candidates, new_last_scanned) = scanner2.scan(Some(0)).await.unwrap();

        // Indices 1 and 2 are now scanned successfully
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 1);
        assert_eq!(candidates[1].index, 2);
        assert_eq!(new_last_scanned, Some(2));
    }

    /// Games with a non-zero TEE prover but zero ZK prover are still candidates.
    ///
    /// The scanner currently filters only on `zk_prover`; a TEE proof alone does
    /// not mark a game as challenged. This test guards that behaviour so a future
    /// change to the filtering logic will surface as a test failure.
    #[tokio::test]
    async fn test_scan_tee_prover_nonzero_still_candidate() {
        let tee_addr = Address::repeat_byte(0xEE);

        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 1), factory_game(1, 1)],
        });

        let mut verifier_games = HashMap::new();
        // Game 0: IN_PROGRESS, no ZK prover, but has a TEE prover -> still a candidate
        verifier_games.insert(addr(0), mock_state_with_tee(0, Address::ZERO, tee_addr, 100));
        // Game 1: IN_PROGRESS, no ZK prover, no TEE prover -> candidate
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[1].index, 1);
        assert_eq!(new_last_scanned, Some(1));
    }

    /// Error at the first index (0) with `last_scanned = None` preserves fresh-start semantics.
    #[tokio::test]
    async fn test_scan_error_at_first_index() {
        let factory = Arc::new(ErrorOnIndexFactory {
            inner: MockDisputeGameFactory { games: vec![factory_game(0, 1), factory_game(1, 1)] },
            error_indices: vec![0],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        // last_scanned = None, lowest_error = 0 -> preserves None (fresh-start semantics)
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
        assert_eq!(new_last_scanned, None);
    }
}
