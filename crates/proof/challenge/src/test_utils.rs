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
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bloom, Bytes, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header as RpcHeader, TransactionReceipt};
use alloy_trie::{HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};
use async_trait::async_trait;
use base_common_consensus::Predeploys;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorPreflight, AnchorRoot, AnchorSnapshot,
    AnchorStateRegistryClient, ContractError, DisputeGameFactoryClient, GameAtIndex, GameInfo,
    GameStatus,
};
use base_proof_rpc::{BaseHeader, L1Provider, L2Provider, RpcError, RpcResult};
use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
use base_prover_service_protocol::{
    DeleteProofRequest, GetProofRequest, GetProofResponse, ProofResult as ApiProofResult,
    ProofStatus, ProveBlockRangeRequest, ProveBlockRangeResponse, SnarkPlonkProofResult,
    ZkProofResult, ZkVm,
};
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};

/// Discovery interval used in tests (5 minutes).
pub const TEST_DISCOVERY_INTERVAL: Duration = Duration::from_secs(300);

/// Per-game state for the mock verifier.
#[derive(Debug, Clone)]
pub struct MockGameState {
    /// Game status.
    pub status: GameStatus,
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
    /// Address of the `DelayedWETH` contract.
    pub delayed_weth: Address,
    /// Address of the `AnchorStateRegistry` contract.
    pub anchor_state_registry: Address,
    /// Whether the game is finalized in the `AnchorStateRegistry`.
    pub is_finalized: bool,
    /// Current anchor root returned by the `AnchorStateRegistry`.
    pub anchor_root: AnchorRoot,
}

impl Default for MockGameState {
    fn default() -> Self {
        Self {
            status: GameStatus::InProgress,
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
            delayed_weth: Address::ZERO,
            anchor_state_registry: Address::ZERO,
            is_finalized: true,
            anchor_root: AnchorRoot { root: B256::ZERO, l2_block_number: 0 },
        }
    }
}

/// Mock dispute game factory with configurable per-index game data.
///
/// The game list lives behind a `Mutex` so multi-step tests can extend it
/// after the scanner has been constructed (e.g. simulating new games being
/// added between scan ticks).
#[derive(Debug)]
pub struct MockDisputeGameFactory {
    /// Ordered list of games in the factory.
    pub games: Mutex<Vec<GameAtIndex>>,
    /// Games keyed by `(game_type, root_claim, extra_data)` for UUID lookups.
    pub uuid_games: Mutex<HashMap<(u32, B256, Bytes), Address>>,
}

impl MockDisputeGameFactory {
    /// Creates a new mock from an initial set of games.
    pub fn new(games: Vec<GameAtIndex>) -> Self {
        Self { games: Mutex::new(games), uuid_games: Mutex::new(HashMap::new()) }
    }

    /// Appends a single game to the factory.
    pub fn push(&self, game: GameAtIndex) {
        self.games.lock().unwrap().push(game);
    }

    /// Inserts a UUID-addressable game.
    pub fn insert_uuid_game(
        &self,
        game_type: u32,
        root_claim: B256,
        extra_data: Bytes,
        proxy: Address,
    ) {
        self.uuid_games.lock().unwrap().insert((game_type, root_claim, extra_data), proxy);
    }
}

#[async_trait]
impl DisputeGameFactoryClient for MockDisputeGameFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        Ok(self.games.lock().unwrap().len() as u64)
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        self.games
            .lock()
            .unwrap()
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
        game_type: u32,
        root_claim: B256,
        extra_data: Bytes,
    ) -> Result<Address, ContractError> {
        Ok(self
            .uuid_games
            .lock()
            .unwrap()
            .get(&(game_type, root_claim, extra_data))
            .copied()
            .unwrap_or(Address::ZERO))
    }
}

/// Mock anchor-state registry with a mutable anchor snapshot.
#[derive(Debug)]
pub struct MockAnchorStateRegistry {
    /// Current anchor snapshot returned by the mock.
    pub snapshot: Mutex<AnchorSnapshot>,
    /// Whether `anchor_snapshot` should return a simulated error.
    pub fail_snapshot: Mutex<bool>,
}

impl MockAnchorStateRegistry {
    /// Creates a new mock with the given anchor game.
    pub const fn new(anchor_game: Address) -> Self {
        Self {
            snapshot: Mutex::new(AnchorSnapshot {
                anchor_root: AnchorRoot { root: B256::ZERO, l2_block_number: 0 },
                anchor_game,
            }),
            fail_snapshot: Mutex::new(false),
        }
    }

    /// Updates the current anchor game.
    pub fn set_anchor_game(&self, anchor_game: Address) {
        self.snapshot.lock().unwrap().anchor_game = anchor_game;
    }

    /// Sets whether `anchor_snapshot` should return a simulated error.
    pub fn set_fail_snapshot(&self, fail_snapshot: bool) {
        *self.fail_snapshot.lock().unwrap() = fail_snapshot;
    }
}

#[async_trait]
impl AnchorStateRegistryClient for MockAnchorStateRegistry {
    async fn anchor_snapshot(&self) -> Result<AnchorSnapshot, ContractError> {
        if *self.fail_snapshot.lock().unwrap() {
            return Err(ContractError::Validation("simulated anchor snapshot error".to_owned()));
        }
        Ok(*self.snapshot.lock().unwrap())
    }
}

/// Helper to create a mock anchor-state registry behind an [`Arc`].
pub fn mock_anchor_registry(anchor_game: Address) -> Arc<dyn AnchorStateRegistryClient> {
    Arc::new(MockAnchorStateRegistry::new(anchor_game))
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
    /// Addresses passed to `game_info`, used by tests that assert cached reads.
    pub game_info_reads: Mutex<Vec<Address>>,
    /// Addresses passed to `status`, used by tests that assert cached reads.
    pub status_reads: Mutex<Vec<Address>>,
    /// Addresses passed to `delayed_weth`, used by tests that assert cached reads.
    pub delayed_weth_reads: Mutex<Vec<Address>>,
    /// Addresses passed to the interval reads, used by tests that assert read counts.
    pub intermediate_block_interval_reads: Mutex<Vec<Address>>,
    /// `(block_interval, intermediate_block_interval)` returned for starting blocks
    /// below `denim_activation_block`.
    pub intervals: (u64, u64),
    /// First starting block that resolves to `denim_intervals`.
    pub denim_activation_block: u64,
    /// `(block_interval, intermediate_block_interval)` returned at or after
    /// `denim_activation_block`.
    pub denim_intervals: (u64, u64),
}

impl MockAggregateVerifier {
    /// Creates a new mock verifier from a pre-built game state map.
    pub const fn new(games: HashMap<Address, MockGameState>) -> Self {
        Self {
            games: Mutex::new(games),
            game_info_reads: Mutex::new(Vec::new()),
            status_reads: Mutex::new(Vec::new()),
            delayed_weth_reads: Mutex::new(Vec::new()),
            intermediate_block_interval_reads: Mutex::new(Vec::new()),
            intervals: (10, 5),
            denim_activation_block: u64::MAX,
            denim_intervals: (10, 5),
        }
    }

    /// Sets the interval pair returned for every starting block before Denim.
    #[must_use]
    pub const fn with_intervals(
        mut self,
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> Self {
        self.intervals = (block_interval, intermediate_block_interval);
        self
    }

    /// Makes the verifier switch to `(block_interval, intermediate_block_interval)` for
    /// games whose range starts at or after `activation_block`, as the Denim-aware
    /// `AggregateVerifier` does.
    #[must_use]
    pub const fn with_denim_intervals(
        mut self,
        activation_block: u64,
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> Self {
        self.denim_activation_block = activation_block;
        self.denim_intervals = (block_interval, intermediate_block_interval);
        self
    }

    /// Updates the state for a specific game address.
    ///
    /// Multi-step driver tests call this between steps to simulate on-chain
    /// state changes (e.g. marking a game as resolved after proof submission).
    pub fn update_game(&self, address: Address, state: MockGameState) {
        self.games.lock().unwrap().insert(address, state);
    }

    /// Returns how many times `game_info` was read for a game.
    pub fn game_info_read_count(&self, game_address: Address) -> usize {
        self.game_info_reads
            .lock()
            .unwrap()
            .iter()
            .filter(|&&read_address| read_address == game_address)
            .count()
    }

    /// Returns how many times `status` was read for a game.
    pub fn status_read_count(&self, game_address: Address) -> usize {
        self.status_reads
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
        self.game_info_reads.lock().unwrap().push(game_address);
        self.get(game_address, |s| s.game_info)
    }

    async fn game_type(&self, _game_address: Address) -> Result<u32, ContractError> {
        Ok(1)
    }

    async fn status(&self, game_address: Address) -> Result<GameStatus, ContractError> {
        self.status_reads.lock().unwrap().push(game_address);
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
        Ok(self.intervals.0)
    }

    async fn read_intermediate_block_interval(
        &self,
        impl_address: Address,
    ) -> Result<u64, ContractError> {
        self.intermediate_block_interval_reads.lock().unwrap().push(impl_address);
        Ok(self.intervals.1)
    }

    async fn read_intervals_for_starting_block(
        &self,
        impl_address: Address,
        starting_block: u64,
    ) -> Result<(u64, u64), ContractError> {
        self.intermediate_block_interval_reads.lock().unwrap().push(impl_address);
        if starting_block < self.denim_activation_block {
            Ok(self.intervals)
        } else {
            Ok(self.denim_intervals)
        }
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
        self.get(game_address, |s| s.bond_recipient)
    }

    async fn bond_unlocked(&self, game_address: Address) -> Result<bool, ContractError> {
        self.get(game_address, |s| s.bond_unlocked)
    }

    async fn bond_claimed(&self, game_address: Address) -> Result<bool, ContractError> {
        self.get(game_address, |s| s.bond_claimed)
    }

    async fn expected_resolution(&self, game_address: Address) -> Result<u64, ContractError> {
        self.get(game_address, |_| u64::MAX)
    }

    async fn proof_count(&self, game_address: Address) -> Result<u8, ContractError> {
        self.get(game_address, |_| 0)
    }

    async fn created_at(&self, game_address: Address) -> Result<u64, ContractError> {
        self.get(game_address, |_| 0)
    }

    async fn delayed_weth(&self, game_address: Address) -> Result<Address, ContractError> {
        self.delayed_weth_reads.lock().unwrap().push(game_address);
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
            blacklisted: false,
            retired: false,
            respected: true,
            paused: false,
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
pub fn mock_state(status: GameStatus, zk_prover: Address, block_number: u64) -> MockGameState {
    mock_state_with_tee(status, zk_prover, DEFAULT_TEE_PROVER, block_number)
}

/// Helper to build mock game state with an explicit TEE prover address.
pub fn mock_state_with_tee(
    status: GameStatus,
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
        ..Default::default()
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
    /// Delay applied before returning a header.
    pub header_delay: Option<Duration>,
}

impl MockL2Provider {
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

    async fn header_by_number(&self, block: BlockNumberOrTag) -> RpcResult<BaseHeader> {
        let block_number = match block {
            BlockNumberOrTag::Number(number) => number,
            other => panic!("MockL2Provider::header_by_number does not support tag {other:?}"),
        };
        if let Some(delay) = self.header_delay {
            tokio::time::sleep(delay).await;
        }
        if self.error_blocks.contains(&block_number) {
            return Err(RpcError::BlockNotFound(format!("block {block_number} not available")));
        }
        self.headers
            .get(&block_number)
            .cloned()
            .map(Into::into)
            .ok_or_else(|| RpcError::HeaderNotFound(format!("no header for block {block_number}")))
    }

    async fn block_by_number(
        &self,
        _block: BlockNumberOrTag,
    ) -> RpcResult<base_proof_rpc::BaseBlock> {
        Err(RpcError::BlockNotFound("not implemented in mock".into()))
    }

    async fn block_by_hash(&self, _hash: B256) -> RpcResult<base_proof_rpc::BaseBlock> {
        Err(RpcError::BlockNotFound("not implemented in mock".into()))
    }
}

/// Mock prover-service requester for testing ZK proof flows in the driver.
#[derive(Debug, Default)]
pub struct MockZkProofProvider {
    /// Mutable proof state returned by [`get_proof`](ProofRequesterProvider::get_proof).
    pub state: Mutex<MockZkProofState>,
}

/// Mutable state for [`MockZkProofProvider`].
#[derive(Debug, Clone)]
pub struct MockZkProofState {
    /// Proof status returned by [`get_proof`](ProofRequesterProvider::get_proof).
    pub proof_status: ProofStatus,
    /// Proof bytes returned when status is [`ProofStatus::Succeeded`].
    pub proof: Vec<u8>,
    /// Optional full prover-service result returned when status is [`ProofStatus::Succeeded`].
    pub result: Option<ApiProofResult>,
    /// When `true`, [`get_proof`](ProofRequesterProvider::get_proof) returns `None` for
    /// `result` even when `proof_status` is [`ProofStatus::Succeeded`]. Used to simulate
    /// the "succeeded without result" malformed-response path.
    pub omit_result_on_success: bool,
    /// Error message returned when status is `Failed`.
    pub error_message: Option<String>,
    /// Every [`ProveBlockRangeRequest`] received by `prove_block_range`, in call order.
    pub prove_block_range_log: Vec<ProveBlockRangeRequest>,
}

impl Default for MockZkProofState {
    fn default() -> Self {
        Self {
            proof_status: ProofStatus::Queued,
            proof: Vec::new(),
            result: None,
            omit_result_on_success: false,
            error_message: None,
            prove_block_range_log: Vec::new(),
        }
    }
}

#[async_trait]
impl ProofRequesterProvider for MockZkProofProvider {
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
        let session_id = request.proof.session_id.clone();
        self.state.lock().unwrap().prove_block_range_log.push(request);
        Ok(ProveBlockRangeResponse { session_id })
    }

    async fn get_proof(
        &self,
        _request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        let state = self.state.lock().unwrap().clone();
        let result = if state.proof_status == ProofStatus::Succeeded {
            if state.omit_result_on_success {
                None
            } else {
                state.result.or_else(|| {
                    Some(ApiProofResult::SnarkPlonk(SnarkPlonkProofResult {
                        proof: ZkProofResult {
                            zk_vm: ZkVm::Sp1,
                            proof: state.proof.into(),
                            execution_stats: None,
                        },
                    }))
                })
            }
        } else {
            None
        };
        Ok(GetProofResponse {
            status: state.proof_status,
            error_message: state.error_message,
            result,
        })
    }

    async fn delete_proof_request(
        &self,
        request: DeleteProofRequest,
    ) -> Result<(), ProverServiceClientError> {
        let mut state = self.state.lock().unwrap();
        state.prove_block_range_log.retain(|entry| entry.proof.session_id != request.session_id);
        Ok(())
    }

    async fn delete_proofs_by_tee_signer(
        &self,
        _request: base_prover_service_protocol::DeleteProofsByTeeSignerRequest,
    ) -> Result<u64, ProverServiceClientError> {
        unimplemented!("tests do not delete proofs by tee signer")
    }

    async fn list_proofs(
        &self,
        _request: base_prover_service_protocol::ListProofsRequest,
    ) -> Result<base_prover_service_protocol::ListProofsResponse, ProverServiceClientError> {
        unimplemented!("tests do not list proofs")
    }
}

/// Mock L1 provider for testing the driver.
#[derive(Debug)]
pub struct MockL1 {
    /// Headers returned by [`L1Provider::header_by_hash`].
    pub headers_by_hash: HashMap<B256, RpcHeader>,
    /// Error returned by [`L1Provider::header_by_hash`], when configured.
    pub header_error: Option<String>,
    /// Hashes requested through [`L1Provider::header_by_hash`].
    pub header_by_hash_requests: Mutex<Vec<B256>>,
}

impl MockL1 {
    /// Creates a mock that returns a header with `number` for `hash`.
    pub fn success(hash: B256, number: u64) -> Self {
        Self {
            headers_by_hash: HashMap::from([(
                hash,
                RpcHeader {
                    hash,
                    inner: ConsensusHeader { number, ..Default::default() },
                    ..Default::default()
                },
            )]),
            header_error: None,
            header_by_hash_requests: Mutex::new(Vec::new()),
        }
    }

    /// Creates a mock that returns a single error.
    pub fn failure(msg: &str) -> Self {
        Self {
            headers_by_hash: HashMap::new(),
            header_error: Some(msg.to_owned()),
            header_by_hash_requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl L1Provider for MockL1 {
    async fn block_number(&self) -> RpcResult<u64> {
        unimplemented!("tests only use header_by_hash")
    }

    async fn header_by_number(&self, _: BlockNumberOrTag) -> RpcResult<RpcHeader> {
        unimplemented!("tests only use header_by_hash")
    }

    async fn header_by_hash(&self, hash: B256) -> RpcResult<RpcHeader> {
        self.header_by_hash_requests.lock().unwrap().push(hash);
        if let Some(error) = &self.header_error {
            return Err(RpcError::HeaderNotFound(error.clone()));
        }
        self.headers_by_hash
            .get(&hash)
            .cloned()
            .ok_or_else(|| RpcError::HeaderNotFound(format!("mock: no header for hash {hash}")))
    }

    async fn block_receipts(&self, _: B256) -> RpcResult<Vec<TransactionReceipt>> {
        unimplemented!("tests only use header_by_hash")
    }

    async fn code_at(&self, _: Address, _: BlockNumberOrTag) -> RpcResult<Bytes> {
        unimplemented!("tests only use header_by_hash")
    }

    async fn call_contract(&self, _: Address, _: Bytes, _: BlockNumberOrTag) -> RpcResult<Bytes> {
        unimplemented!("tests only use header_by_hash")
    }

    async fn get_balance(&self, _: Address) -> RpcResult<U256> {
        unimplemented!("tests only use header_by_hash")
    }
}

/// Mock transaction manager for testing the driver and submitter.
#[derive(Debug, Clone)]
pub struct MockTxManager {
    /// Queue of responses returned by [`send`](TxManager::send).
    pub responses: Arc<Mutex<VecDeque<SendResponse>>>,
    /// Transaction candidates submitted through [`send`](TxManager::send).
    pub calls: Arc<Mutex<Vec<TxCandidate>>>,
}

impl MockTxManager {
    /// Creates a new mock with a single pre-configured response.
    pub fn new(response: SendResponse) -> Self {
        Self::with_responses(vec![response])
    }

    /// Creates a new mock with multiple responses returned in order.
    pub fn with_responses(responses: Vec<SendResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns the recorded transaction candidates.
    pub fn recorded_calls(&self) -> Vec<TxCandidate> {
        self.calls.lock().unwrap().clone()
    }
}

impl TxManager for MockTxManager {
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        self.calls.lock().unwrap().push(candidate);
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
    build_test_header_and_account_for_address(
        block_number,
        storage_hash,
        Predeploys::L2_TO_L1_MESSAGE_PASSER,
    )
}

/// Builds a consensus header and account proof response pair for `address`.
/// The returned header's `state_root` is the trie root that the account proof
/// verifies against.
pub fn build_test_header_and_account_for_address(
    block_number: u64,
    storage_hash: B256,
    address: Address,
) -> (ConsensusHeader, EIP1186AccountProofResponse) {
    let account = TrieAccount {
        nonce: 0,
        balance: U256::ZERO,
        storage_root: storage_hash,
        code_hash: B256::ZERO,
    };
    let mut encoded = Vec::with_capacity(account.length());
    account.encode(&mut encoded);

    let account_key = Nibbles::unpack(keccak256(address));
    let mut hb = HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![account_key]));
    hb.add_leaf(account_key, &encoded);
    let state_root = hb.root();
    let proof_nodes = hb.take_proof_nodes();
    let account_proof: Vec<Bytes> =
        proof_nodes.into_nodes_sorted().into_iter().map(|(_, v)| v).collect();

    let header = ConsensusHeader { number: block_number, state_root, ..Default::default() };
    let account_result = EIP1186AccountProofResponse {
        address,
        account_proof,
        balance: U256::ZERO,
        code_hash: B256::ZERO,
        nonce: 0,
        storage_hash,
        storage_proof: vec![],
    };
    (header, account_result)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[tokio::test]
    #[should_panic(expected = "MockL2Provider::header_by_number does not support tag finalized")]
    async fn test_mock_l2_provider_rejects_block_tags() {
        let provider = MockL2Provider::default();

        let _ = provider.header_by_number(BlockNumberOrTag::Finalized).await;
    }
}
