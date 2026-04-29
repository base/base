//! Test utilities: mock stubs for contract clients, ZK proof provider, tx manager, and scanner
//! tests.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_consensus::{
    Eip658Value, Header as ConsensusHeader, Receipt, ReceiptEnvelope, ReceiptWithBloom,
};
use alloy_primitives::{Address, B256, Bloom, Bytes, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header as RpcHeader, TransactionReceipt};
use alloy_trie::{HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};
use async_trait::async_trait;
use base_common_consensus::Predeploys;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorPreflight, AnchorRoot, ContractError, DisputeGameFactoryClient,
    GameAtIndex, GameInfo,
};
use base_proof_primitives::{ProofRequest, ProofResult, ProverClient};
use base_proof_rpc::{L2Provider, RpcError, RpcResult};
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};
use base_zk_client::{
    GetProofRequest, GetProofResponse, ProveBlockRequest, ProveBlockResponse, ZkProofError,
    ZkProofProvider,
};

use crate::L1HeadProvider;

/// Discovery interval used in tests (5 minutes).
pub const TEST_DISCOVERY_INTERVAL: Duration = Duration::from_secs(300);

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
    /// L1 head block hash stored at game creation time.
    pub l1_head: B256,
    /// Intermediate output roots for this game.
    pub intermediate_output_roots: Vec<B256>,
    /// 1-based index of the challenged intermediate root (`0` = unchallenged).
    pub countered_index: u64,
    /// Whether the game's dispute period has elapsed.
    pub game_over: bool,
    /// Timestamp at which the game was resolved (`0` if unresolved).
    pub resolved_at: u64,
    /// Address that will receive the bond.
    pub bond_recipient: Address,
    /// Whether the bond has been unlocked.
    pub bond_unlocked: bool,
    /// Whether the bond has been claimed.
    pub bond_claimed: bool,
    /// Expected resolution timestamp.
    pub expected_resolution: u64,
    /// Number of verified proofs.
    pub proof_count: u8,
    /// Timestamp at which the game was created.
    pub created_at: u64,
    /// Address of the `DelayedWETH` contract.
    pub delayed_weth: Address,
    /// Address of the `AnchorStateRegistry` contract.
    pub anchor_state_registry: Address,
    /// Whether the game is blacklisted in the `AnchorStateRegistry`.
    pub is_blacklisted: bool,
    /// Whether the game is finalized in the `AnchorStateRegistry`.
    pub is_finalized: bool,
    /// Whether the game is respected in the `AnchorStateRegistry`.
    pub is_respected: bool,
    /// Whether the game is retired in the `AnchorStateRegistry`.
    pub is_retired: bool,
    /// Current anchor root returned by the `AnchorStateRegistry`.
    pub anchor_root: AnchorRoot,
}

impl Default for MockGameState {
    fn default() -> Self {
        Self {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: Address::ZERO,
            game_info: GameInfo {
                root_claim: B256::ZERO,
                l2_block_number: 0,
                parent_address: Address::ZERO,
            },
            starting_block_number: 0,
            l1_head: B256::ZERO,
            intermediate_output_roots: vec![],
            countered_index: 0,
            game_over: false,
            resolved_at: 0,
            bond_recipient: Address::ZERO,
            bond_unlocked: false,
            bond_claimed: false,
            expected_resolution: u64::MAX,
            proof_count: 0,
            created_at: 0,
            delayed_weth: Address::ZERO,
            anchor_state_registry: Address::ZERO,
            is_blacklisted: false,
            is_finalized: true,
            is_respected: true,
            is_retired: false,
            anchor_root: AnchorRoot { root: B256::ZERO, l2_block_number: 0 },
        }
    }
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
            .copied()
            .ok_or_else(|| ContractError::Validation(format!("index {index} out of bounds")))
    }

    async fn init_bonds(&self, _game_type: u32) -> Result<U256, ContractError> {
        Ok(U256::ZERO)
    }

    async fn game_impls(&self, _game_type: u32) -> Result<Address, ContractError> {
        Ok(Address::repeat_byte(0x11))
    }

    async fn games(
        &self,
        _game_type: u32,
        _root_claim: B256,
        _extra_data: Bytes,
    ) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
}

/// Mock aggregate verifier with configurable per-address game state.
///
/// Uses interior mutability (`Mutex`) so that multi-step driver tests can
/// update game state between steps to simulate on-chain effects (e.g.
/// setting `status = 1` after a successful challenge transaction).
#[derive(Debug)]
pub struct MockAggregateVerifier {
    /// Per-address game state lookup, wrapped in a `Mutex` for interior
    /// mutability in multi-step tests.
    pub games: Mutex<HashMap<Address, MockGameState>>,
    /// Addresses passed to `bond_recipient`, used by tests that assert polling
    /// avoids redundant lifecycle reads.
    pub bond_recipient_reads: Mutex<Vec<Address>>,
}

impl MockAggregateVerifier {
    /// Creates a new mock verifier from a pre-built game state map.
    pub const fn new(games: HashMap<Address, MockGameState>) -> Self {
        Self { games: Mutex::new(games), bond_recipient_reads: Mutex::new(Vec::new()) }
    }

    /// Updates the state for a specific game address.
    ///
    /// Multi-step driver tests call this between steps to simulate on-chain
    /// state changes (e.g. marking a game as resolved after proof submission).
    pub fn update_game(&self, address: Address, state: MockGameState) {
        self.games.lock().unwrap().insert(address, state);
    }

    /// Returns how many times `bond_recipient` was read for a game.
    pub fn bond_recipient_read_count(&self, game_address: Address) -> usize {
        self.bond_recipient_reads
            .lock()
            .unwrap()
            .iter()
            .filter(|&&read_address| read_address == game_address)
            .count()
    }

    fn get<T>(
        &self,
        game_address: Address,
        f: impl FnOnce(&MockGameState) -> T,
    ) -> Result<T, ContractError> {
        self.games
            .lock()
            .unwrap()
            .get(&game_address)
            .map(f)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }
}

#[async_trait]
impl AggregateVerifierClient for MockAggregateVerifier {
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ContractError> {
        self.get(game_address, |s| s.game_info)
    }

    async fn status(&self, game_address: Address) -> Result<u8, ContractError> {
        self.get(game_address, |s| s.status)
    }

    async fn zk_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        self.get(game_address, |s| s.zk_prover)
    }

    async fn tee_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        self.get(game_address, |s| s.tee_prover)
    }

    async fn starting_block_number(&self, game_address: Address) -> Result<u64, ContractError> {
        self.get(game_address, |s| s.starting_block_number)
    }

    async fn l1_head(&self, game_address: Address) -> Result<B256, ContractError> {
        self.get(game_address, |s| s.l1_head)
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
        self.get(game_address, |s| s.intermediate_output_roots.clone())
    }

    async fn intermediate_output_root(
        &self,
        game_address: Address,
        index: u64,
    ) -> Result<B256, ContractError> {
        self.get(game_address, |s| {
            let idx = index as usize;
            s.intermediate_output_roots
                .get(idx)
                .copied()
                .expect("intermediate_output_root: index out of bounds")
        })
    }

    async fn countered_index(&self, game_address: Address) -> Result<u64, ContractError> {
        self.get(game_address, |s| s.countered_index)
    }

    async fn game_over(&self, game_address: Address) -> Result<bool, ContractError> {
        self.get(game_address, |s| s.game_over)
    }

    async fn resolved_at(&self, game_address: Address) -> Result<u64, ContractError> {
        self.get(game_address, |s| s.resolved_at)
    }

    async fn bond_recipient(&self, game_address: Address) -> Result<Address, ContractError> {
        self.bond_recipient_reads.lock().unwrap().push(game_address);
        self.get(game_address, |s| s.bond_recipient)
    }

    async fn bond_unlocked(&self, game_address: Address) -> Result<bool, ContractError> {
        self.get(game_address, |s| s.bond_unlocked)
    }

    async fn bond_claimed(&self, game_address: Address) -> Result<bool, ContractError> {
        self.get(game_address, |s| s.bond_claimed)
    }

    async fn expected_resolution(&self, game_address: Address) -> Result<u64, ContractError> {
        self.get(game_address, |s| s.expected_resolution)
    }

    async fn proof_count(&self, game_address: Address) -> Result<u8, ContractError> {
        self.get(game_address, |s| s.proof_count)
    }

    async fn created_at(&self, game_address: Address) -> Result<u64, ContractError> {
        self.get(game_address, |s| s.created_at)
    }

    async fn delayed_weth(&self, game_address: Address) -> Result<Address, ContractError> {
        self.get(game_address, |s| s.delayed_weth)
    }

    async fn anchor_state_registry(&self, game_address: Address) -> Result<Address, ContractError> {
        self.get(game_address, |s| s.anchor_state_registry)
    }

    async fn is_game_finalized(
        &self,
        asr_address: Address,
        game_address: Address,
    ) -> Result<bool, ContractError> {
        let games = self.games.lock().unwrap();
        let state = games.get(&game_address).ok_or_else(|| {
            ContractError::Validation(format!("mock: no state for game {game_address}"))
        })?;
        if state.anchor_state_registry != asr_address {
            return Err(ContractError::Validation(format!(
                "mock: game {game_address} has ASR {} but caller passed {asr_address}",
                state.anchor_state_registry
            )));
        }
        Ok(state.is_finalized)
    }

    async fn anchor_preflight(
        &self,
        asr_address: Address,
        game_address: Address,
    ) -> Result<AnchorPreflight, ContractError> {
        let games = self.games.lock().unwrap();
        let state = games.get(&game_address).ok_or_else(|| {
            ContractError::Validation(format!("mock: no state for game {game_address}"))
        })?;
        if state.anchor_state_registry != asr_address {
            return Err(ContractError::Validation(format!(
                "mock: game {game_address} has ASR {} but caller passed {asr_address}",
                state.anchor_state_registry
            )));
        }
        Ok(AnchorPreflight {
            blacklisted: state.is_blacklisted,
            retired: state.is_retired,
            respected: state.is_respected,
            anchor_root: state.anchor_root,
        })
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

/// Helper to create an empty [`MockDisputeGameFactory`] behind an `Arc`.
pub fn empty_factory() -> Arc<dyn DisputeGameFactoryClient> {
    Arc::new(MockDisputeGameFactory { games: vec![] })
}

/// Default TEE prover address used by [`mock_state`].
///
/// Every game in the multiproof system is initialized with at least one
/// prover, so the default mock state uses a non-zero TEE prover to match
/// the production invariant.
pub const DEFAULT_TEE_PROVER: Address = Address::new([0xEE; 20]);

/// Default L1 head hash used by [`mock_state`].
pub const DEFAULT_L1_HEAD: B256 = B256::repeat_byte(0xAA);

/// Helper to build mock game state for the verifier.
///
/// Uses [`DEFAULT_TEE_PROVER`] as the TEE prover address. Use
/// [`mock_state_with_tee`] to override.
pub const fn mock_state(status: u8, zk_prover: Address, block_number: u64) -> MockGameState {
    mock_state_with_tee(status, zk_prover, DEFAULT_TEE_PROVER, block_number)
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
            parent_address: Address::ZERO,
        },
        starting_block_number: block_number.saturating_sub(10),
        l1_head: DEFAULT_L1_HEAD,
        intermediate_output_roots: vec![],
        countered_index: 0,
        game_over: false,
        resolved_at: 0,
        bond_recipient: Address::ZERO,
        bond_unlocked: false,
        bond_claimed: false,
        expected_resolution: u64::MAX,
        proof_count: 0,
        created_at: 0,
        delayed_weth: Address::ZERO,
        anchor_state_registry: Address::ZERO,
        is_blacklisted: false,
        is_finalized: true,
        is_respected: true,
        is_retired: false,
        anchor_root: AnchorRoot { root: B256::ZERO, l2_block_number: 0 },
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

    async fn games(
        &self,
        game_type: u32,
        root_claim: B256,
        extra_data: Bytes,
    ) -> Result<Address, ContractError> {
        self.inner.games(game_type, root_claim, extra_data).await
    }
}

/// Mock L2 provider with configurable block headers and storage proofs.
///
/// Returns pre-configured headers by block number and account proofs by
/// block hash. Block numbers in `error_blocks` will return a
/// [`RpcError::BlockNotFound`] to simulate missing blocks.
#[derive(Debug, Default)]
pub struct MockL2Provider {
    /// Headers keyed by block number.
    pub headers: HashMap<u64, RpcHeader>,
    /// Account proofs keyed by block hash.
    pub proofs: HashMap<B256, EIP1186AccountProofResponse>,
    /// Block numbers that should return an error (simulating missing blocks).
    pub error_blocks: Vec<u64>,
}

impl MockL2Provider {
    /// Creates a new empty mock L2 provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a block header and corresponding account proof.
    ///
    /// The consensus header is wrapped in an RPC header with the hash computed
    /// from [`ConsensusHeader::hash_slow`].
    pub fn insert_block(
        &mut self,
        block_number: u64,
        consensus_header: ConsensusHeader,
        account_result: EIP1186AccountProofResponse,
    ) {
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };
        self.headers.insert(block_number, rpc_header);
        self.proofs.insert(block_hash, account_result);
    }
}

#[async_trait]
impl L2Provider for MockL2Provider {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }

    async fn get_proof(
        &self,
        _address: Address,
        block_hash: B256,
    ) -> RpcResult<EIP1186AccountProofResponse> {
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

    async fn block_by_number(&self, _number: Option<u64>) -> RpcResult<base_proof_rpc::BaseBlock> {
        Err(RpcError::BlockNotFound("not implemented in mock".into()))
    }

    async fn block_by_hash(&self, _hash: B256) -> RpcResult<base_proof_rpc::BaseBlock> {
        Err(RpcError::BlockNotFound("not implemented in mock".into()))
    }
}

/// Mock ZK proof provider for testing the driver.
#[derive(Debug)]
pub struct MockZkProofProvider {
    /// Session ID returned by [`prove_block`](ZkProofProvider::prove_block).
    pub session_id: String,
    /// Mutable proof state returned by [`get_proof`](ZkProofProvider::get_proof).
    pub state: Mutex<MockZkProofState>,
}

/// Mutable state for [`MockZkProofProvider`].
#[derive(Debug, Default, Clone)]
pub struct MockZkProofState {
    /// Proof job status returned by [`get_proof`](ZkProofProvider::get_proof).
    pub proof_status: i32,
    /// Proof receipt bytes returned when status is `Succeeded`.
    pub receipt: Vec<u8>,
    /// Error message returned when status is `Failed`.
    pub error_message: Option<String>,
}

impl Default for MockZkProofProvider {
    fn default() -> Self {
        Self { session_id: String::new(), state: Mutex::new(MockZkProofState::default()) }
    }
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
        let state = self.state.lock().unwrap().clone();
        Ok(GetProofResponse {
            status: state.proof_status,
            receipt: state.receipt,
            error_message: state.error_message,
        })
    }
}

/// Mock TEE proof provider for testing the driver.
#[derive(Debug)]
pub struct MockTeeProofProvider {
    /// Queue of results returned by [`prove`](ProverClient::prove).
    pub results: Mutex<VecDeque<Result<ProofResult, Box<dyn std::error::Error + Send + Sync>>>>,
}

impl MockTeeProofProvider {
    /// Creates a mock that returns a single successful result.
    pub fn success(result: ProofResult) -> Self {
        Self { results: Mutex::new(VecDeque::from([Ok(result)])) }
    }

    /// Creates a mock that returns a single error.
    pub fn failure(msg: &str) -> Self {
        Self { results: Mutex::new(VecDeque::from([Err(msg.into())])) }
    }
}

#[async_trait]
impl ProverClient for MockTeeProofProvider {
    async fn prove(
        &self,
        _request: ProofRequest,
    ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
        self.results.lock().unwrap().pop_front().expect("MockTeeProofProvider has no more results")
    }
}

/// Mock L1 head provider for testing the driver.
#[derive(Debug)]
pub struct MockL1HeadProvider {
    /// Queue of `(expected_hash, result)` pairs returned by
    /// [`block_number_by_hash`](L1HeadProvider::block_number_by_hash).
    /// When `expected_hash` is `Some`, the mock asserts that the caller
    /// passes the correct hash.
    pub block_number_results: Mutex<VecDeque<(Option<B256>, eyre::Result<u64>)>>,
}

impl MockL1HeadProvider {
    /// Creates a mock whose [`block_number_by_hash`](L1HeadProvider::block_number_by_hash)
    /// returns `number` and asserts it is called with `hash`.
    pub fn success(hash: B256, number: u64) -> Self {
        Self { block_number_results: Mutex::new(VecDeque::from([(Some(hash), Ok(number))])) }
    }

    /// Creates a mock that returns a single error.
    pub fn failure(msg: &str) -> Self {
        Self {
            block_number_results: Mutex::new(VecDeque::from([(None, Err(eyre::eyre!("{msg}")))])),
        }
    }
}

#[async_trait]
impl L1HeadProvider for MockL1HeadProvider {
    async fn block_number_by_hash(&self, hash: B256) -> eyre::Result<u64> {
        let (expected_hash, result) = self
            .block_number_results
            .lock()
            .unwrap()
            .pop_front()
            .expect("MockL1HeadProvider has no more block_number_by_hash results");
        if let Some(expected) = expected_hash {
            assert_eq!(
                hash, expected,
                "MockL1HeadProvider::block_number_by_hash called with unexpected hash"
            );
        }
        result
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
        Self { responses: Mutex::new(VecDeque::from([response])) }
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

/// Builds a consensus header and account proof response pair with a valid
/// Merkle proof. The returned header's `state_root` is the trie root that
/// the account proof verifies against.
pub fn build_test_header_and_account(
    block_number: u64,
    storage_hash: B256,
) -> (ConsensusHeader, EIP1186AccountProofResponse) {
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
    let account_result = EIP1186AccountProofResponse {
        address: Predeploys::L2_TO_L1_MESSAGE_PASSER,
        account_proof,
        balance: U256::ZERO,
        code_hash: B256::ZERO,
        nonce: 0,
        storage_hash,
        storage_proof: vec![],
    };
    (header, account_result)
}

/// Mock bond transaction submitter for testing the [`BondManager`](crate::BondManager).
///
/// Records all submitted transactions and returns pre-configured responses.
#[derive(Debug)]
pub struct MockBondTransactionSubmitter {
    /// Queue of results returned by [`send_bond_tx`](crate::BondTransactionSubmitter::send_bond_tx).
    pub responses: Mutex<VecDeque<Result<B256, crate::ChallengeSubmitError>>>,
    /// Recorded `(game_address, to, calldata)` tuples for each submitted transaction.
    pub calls: Mutex<Vec<(Address, Address, Bytes)>>,
}

impl MockBondTransactionSubmitter {
    /// Creates a mock that returns a single successful transaction hash.
    pub fn success(tx_hash: B256) -> Self {
        Self { responses: Mutex::new(VecDeque::from([Ok(tx_hash)])), calls: Mutex::new(Vec::new()) }
    }

    /// Creates a mock with multiple responses returned in order.
    pub fn with_responses(responses: Vec<Result<B256, crate::ChallengeSubmitError>>) -> Self {
        Self { responses: Mutex::new(VecDeque::from(responses)), calls: Mutex::new(Vec::new()) }
    }

    /// Returns the recorded calls as `(game_address, to, calldata)` tuples.
    pub fn recorded_calls(&self) -> Vec<(Address, Address, Bytes)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::BondTransactionSubmitter for MockBondTransactionSubmitter {
    async fn send_bond_tx(
        &self,
        game_address: Address,
        to: Address,
        calldata: Bytes,
    ) -> Result<B256, crate::ChallengeSubmitError> {
        self.calls.lock().unwrap().push((game_address, to, calldata));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("MockBondTransactionSubmitter has no more responses")
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::scanner::{GameCategory, GameScanner, ScannerConfig};

    /// Happy path: mixed games, only `IN_PROGRESS` / non-nullified returned.
    #[tokio::test]
    async fn test_scan_happy_path() {
        // Game 0: type 1, IN_PROGRESS, TEE only -> candidate (InvalidTeeProposal)
        // Game 1: type 99, IN_PROGRESS, TEE only -> candidate (all types scanned)
        // Game 2: type 1, status=1 (not in progress) -> skipped
        // Game 3: type 1, IN_PROGRESS, TEE + ZK (dual proof) -> candidate (InvalidDualProposal)
        // Game 4: type 1, IN_PROGRESS, TEE only -> candidate (InvalidTeeProposal)
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

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        // start = max(0, 5-1000) = 0, so games 0..=4 scanned
        // Game 0: TEE only -> candidate. Game 1: TEE only -> candidate.
        // Game 2: status != 0 -> skipped.
        // Game 3: dual proof (TEE+ZK, no challenge) -> candidate.
        // Game 4: TEE only -> candidate.
        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[0].factory.game_type, 1);
        assert_eq!(candidates[0].info.l2_block_number, 100);
        assert_eq!(candidates[1].index, 1);
        assert_eq!(candidates[1].factory.game_type, 99);
        assert_eq!(candidates[1].info.l2_block_number, 150);
        assert_eq!(candidates[2].index, 3);
        assert_eq!(candidates[2].category, GameCategory::InvalidDualProposal);
        assert_eq!(candidates[3].index, 4);
        assert_eq!(candidates[3].factory.game_type, 1);
        assert_eq!(candidates[3].info.l2_block_number, 400);
    }

    /// Dual-proof games (TEE + ZK, no challenge) are now candidates.
    #[tokio::test]
    async fn test_scan_dual_proof_games_are_candidates() {
        let zk_addr = Address::repeat_byte(0xAA);

        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1)],
        });

        let mut verifier_games = HashMap::new();
        // Game 0: TEE + ZK (dual proof, no challenge) -> candidate (InvalidDualProposal)
        verifier_games.insert(addr(0), mock_state(0, zk_addr, 100));
        // Game 1: TEE only -> candidate (InvalidTeeProposal)
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));
        // Game 2: TEE + ZK (dual proof, no challenge) -> candidate (InvalidDualProposal)
        verifier_games.insert(addr(2), mock_state(0, zk_addr, 300));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[0].category, GameCategory::InvalidDualProposal);
        assert_eq!(candidates[1].index, 1);
        assert_eq!(candidates[1].category, GameCategory::InvalidTeeProposal);
        assert_eq!(candidates[2].index, 2);
        assert_eq!(candidates[2].category, GameCategory::InvalidDualProposal);
    }

    /// Empty factory returns empty vec without error.
    #[tokio::test]
    async fn test_scan_empty_factory() {
        let factory = Arc::new(MockDisputeGameFactory { games: vec![] });
        let verifier = Arc::new(MockAggregateVerifier::new(HashMap::new()));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert!(candidates.is_empty());
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
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 3 });

        // start = 100-3 = 97, end = 99
        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].index, 97);
        assert_eq!(candidates[1].index, 98);
        assert_eq!(candidates[2].index, 99);
    }

    /// Error resilience: a per-game error is logged and skipped, other games still returned.
    /// Errored games are naturally retried on the next scan since the full lookback
    /// window is always evaluated.
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

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        // Index 0 -> candidate. Index 1 errors -> skipped. Index 2 -> candidate.
        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[1].index, 2);
    }

    /// Games with a non-zero TEE prover but zero ZK prover are still candidates.
    ///
    /// A non-zero `teeProver` with `zkProver == ZERO` is the normal initial
    /// state for an unchallenged game. The scanner should return these as
    /// candidates.
    #[tokio::test]
    async fn test_scan_tee_prover_nonzero_still_candidate() {
        let tee_addr = Address::repeat_byte(0xEE);

        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 1), factory_game(1, 1)],
        });

        let mut verifier_games = HashMap::new();
        // Game 0: IN_PROGRESS, no ZK prover, has a TEE prover -> candidate
        verifier_games.insert(addr(0), mock_state_with_tee(0, Address::ZERO, tee_addr, 100));
        // Game 1: IN_PROGRESS, no ZK prover, has default TEE prover -> candidate
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[1].index, 1);
    }

    /// Error at the first index (0) skips that game, rest still returned.
    #[tokio::test]
    async fn test_scan_error_at_first_index() {
        let factory = Arc::new(ErrorOnIndexFactory {
            inner: MockDisputeGameFactory { games: vec![factory_game(0, 1), factory_game(1, 1)] },
            error_indices: vec![0],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
    }

    /// A challenged game (TEE + ZK provers non-zero, `countered_index` > 0) is
    /// returned as a [`GameCategory::FraudulentZkChallenge`] candidate.
    #[tokio::test]
    async fn test_scan_challenged_game_returns_fraudulent_zk_challenge() {
        let tee_addr = Address::repeat_byte(0xEE);
        let zk_addr = Address::repeat_byte(0xCC);

        let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });

        let mut verifier_games = HashMap::new();
        let mut state = mock_state_with_tee(0, zk_addr, tee_addr, 100);
        state.countered_index = 3; // 1-based: challenged at 0-based index 2
        verifier_games.insert(addr(0), state);

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].category,
            GameCategory::FraudulentZkChallenge { challenged_index: 2 }
        );
    }

    /// A ZK-proposed game (`tee_prover` == 0, `zk_prover` != 0, unchallenged) is
    /// returned as a [`GameCategory::InvalidZkProposal`] candidate.
    #[tokio::test]
    async fn test_scan_zk_proposal_returns_invalid_zk_proposal() {
        let zk_addr = Address::repeat_byte(0xCC);

        let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });

        let mut verifier_games = HashMap::new();
        // tee_prover == ZERO, zk_prover != ZERO, countered_index == 0
        verifier_games.insert(addr(0), mock_state_with_tee(0, zk_addr, Address::ZERO, 100));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].category, GameCategory::InvalidZkProposal);
    }

    /// A TEE-proposed unchallenged game is returned as
    /// [`GameCategory::InvalidTeeProposal`].
    #[tokio::test]
    async fn test_scan_tee_proposal_returns_invalid_tee_proposal() {
        let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].category, GameCategory::InvalidTeeProposal);
    }

    /// A game with both proofs verified (TEE + ZK, no challenge) is a
    /// candidate for validation. Both proofs may verify a wrong root.
    #[tokio::test]
    async fn test_scan_both_proofs_verified_is_candidate() {
        let tee_addr = Address::repeat_byte(0xEE);
        let zk_addr = Address::repeat_byte(0xCC);

        let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });

        let mut verifier_games = HashMap::new();
        // Both provers non-zero, countered_index == 0 (added via verifyProposalProof)
        verifier_games.insert(addr(0), mock_state_with_tee(0, zk_addr, tee_addr, 100));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1, "dual-proof game should be a candidate");
        assert_eq!(candidates[0].category, GameCategory::InvalidDualProposal);
    }

    /// Games with both `teeProver` and `zkProver` at `Address::ZERO` are
    /// filtered out. Every game is initialized with at least one prover, so
    /// both being zero indicates a prior nullification.
    #[tokio::test]
    async fn test_scan_filters_nullified_games() {
        let tee_addr = Address::repeat_byte(0xEE);

        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1)],
        });

        let mut verifier_games = HashMap::new();
        // Game 0: both provers zeroed (nullified) → filtered out
        verifier_games.insert(addr(0), mock_state_with_tee(0, Address::ZERO, Address::ZERO, 100));
        // Game 1: TEE prover active, ZK prover zero → candidate
        verifier_games.insert(addr(1), mock_state_with_tee(0, Address::ZERO, tee_addr, 200));
        // Game 2: both provers zeroed (nullified) → filtered out
        verifier_games.insert(addr(2), mock_state_with_tee(0, Address::ZERO, Address::ZERO, 300));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let scanner = GameScanner::new(factory, verifier, ScannerConfig { lookback_games: 1000 });

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1, "only the non-nullified game should be a candidate");
        assert_eq!(candidates[0].index, 1);
    }
}
