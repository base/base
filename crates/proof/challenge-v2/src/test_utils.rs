//! Test utilities for `base-challenger-v2`.
//!
//! Gated behind the `test-utils` feature so the heavier deps (`alloy-consensus`,
//! `serde_json`, etc.) are only pulled in for tests and downstream test crates.

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
    AggregateVerifierClient, AnchorPreflight, AnchorRoot, AnchorSnapshot,
    AnchorStateRegistryClient, ContractError, DelayedWETHClient, DisputeGameFactoryClient,
    GameAtIndex, GameInfo as VerifierGameInfo, GameStatus,
};
use base_proof_rpc::{BaseBlock, L2Provider, RpcError, RpcResult};
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};
use base_zk_client::{
    GetProofRequest, GetProofResponse, ProofJobStatus, ProveBlockRequest, ProveBlockResponse,
    ZkProofError, ZkProofProvider,
};

use crate::{
    DelayedWETHResolver, OutputRootError, OutputValidator, TeeProofError, TeeProofProvider,
    TeeProofResult,
};

/// Mock [`L2Provider`] backed by in-memory hashmaps.
///
/// Configure by calling [`MockL2Provider::insert_block`] with a header and
/// matching account proof, or by pushing block numbers into
/// [`MockL2Provider::error_blocks`] to simulate missing blocks.
#[derive(Debug, Default)]
pub struct MockL2Provider {
    /// Headers keyed by block number.
    pub headers: HashMap<u64, RpcHeader>,
    /// Account proofs keyed by block hash.
    pub proofs: HashMap<B256, EIP1186AccountProofResponse>,
    /// Block numbers that should return `BlockNotFound`.
    pub error_blocks: Vec<u64>,
}

impl MockL2Provider {
    /// Returns a new empty mock provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a `(header, account_proof)` pair under the header's slow hash.
    /// The header is wrapped in an `RpcHeader` whose `hash` field matches the
    /// computed consensus hash, so the validator's consistency check passes.
    pub fn insert_block(
        &mut self,
        block_number: u64,
        header: ConsensusHeader,
        proof: EIP1186AccountProofResponse,
    ) {
        let hash = header.hash_slow();
        let rpc_header = RpcHeader { hash, inner: header, ..Default::default() };
        self.headers.insert(block_number, rpc_header);
        self.proofs.insert(hash, proof);
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

    async fn block_by_number(&self, _number: Option<u64>) -> RpcResult<BaseBlock> {
        Err(RpcError::BlockNotFound("block_by_number not implemented in mock".into()))
    }

    async fn block_by_hash(&self, _hash: B256) -> RpcResult<BaseBlock> {
        Err(RpcError::BlockNotFound("block_by_hash not implemented in mock".into()))
    }
}

/// Builds a single-leaf MPT containing the account at `address` with the
/// given fields, returning a `(consensus_header, eth_getProof_response)`
/// pair. The header's `state_root` is the trie root that the proof verifies
/// against.
pub fn build_account_at_block(
    block_number: u64,
    address: Address,
    nonce: u64,
    balance: U256,
    storage_hash: B256,
    code_hash: B256,
) -> (ConsensusHeader, EIP1186AccountProofResponse) {
    let account = TrieAccount { nonce, balance, storage_root: storage_hash, code_hash };
    let mut encoded = Vec::with_capacity(account.length());
    account.encode(&mut encoded);

    let key = Nibbles::unpack(keccak256(address));
    let mut hb = HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![key]));
    hb.add_leaf(key, &encoded);
    let state_root = hb.root();
    let proof_nodes = hb.take_proof_nodes();
    let account_proof: Vec<Bytes> =
        proof_nodes.into_nodes_sorted().into_iter().map(|(_, v)| v).collect();

    let header = ConsensusHeader { number: block_number, state_root, ..Default::default() };
    let response = EIP1186AccountProofResponse {
        address,
        account_proof,
        balance,
        code_hash,
        nonce,
        storage_hash,
        storage_proof: vec![],
    };

    (header, response)
}

/// Convenience helper that calls [`build_account_at_block`] for the
/// `L2ToL1MessagePasser` predeploy with default fields and the given storage
/// hash. The returned header's `state_root` is the trie root that the proof
/// verifies against.
pub fn build_message_passer_proof(
    block_number: u64,
    storage_hash: B256,
) -> (ConsensusHeader, EIP1186AccountProofResponse) {
    build_account_at_block(
        block_number,
        Predeploys::L2_TO_L1_MESSAGE_PASSER,
        0,
        U256::ZERO,
        storage_hash,
        B256::ZERO,
    )
}

/// Mock [`OutputValidator`] backed by an in-memory `block -> root` map.
///
/// Configure via [`MockOutputValidator::set`]. Calls for unconfigured
/// blocks panic so tests fail loudly when assumptions are wrong.
#[derive(Debug, Default)]
pub struct MockOutputValidator {
    /// Output roots keyed by block number.
    pub roots: Mutex<HashMap<u64, B256>>,
}

impl MockOutputValidator {
    /// Creates an empty mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Programs `block_number` to return `root`.
    pub fn set(&self, block_number: u64, root: B256) {
        self.roots.lock().expect("roots lock poisoned").insert(block_number, root);
    }
}

#[async_trait]
impl OutputValidator for MockOutputValidator {
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, OutputRootError> {
        match self.roots.lock().expect("roots lock poisoned").get(&block_number) {
            Some(&root) => Ok(root),
            None => panic!("MockOutputValidator: no root configured for block {block_number}"),
        }
    }
}

/// Per-game state read by [`MockAggregateVerifier`].
#[derive(Debug, Clone)]
pub struct MockGameState {
    /// Status returned by `status()`.
    pub status: GameStatus,
    /// Address returned by `teeProver()`.
    pub tee_prover: Address,
    /// Address returned by `zkProver()`.
    pub zk_prover: Address,
    /// Raw `counteredByIntermediateRootIndexPlusOne()`.
    pub countered_index: u64,
    /// `rootClaim()` (also returned via `game_info`).
    pub root_claim: B256,
    /// `l2SequenceNumber()` (also returned via `game_info`).
    pub l2_block_number: u64,
    /// `parentAddress()` (also returned via `game_info`).
    pub parent_address: Address,
    /// `startingBlockNumber()`.
    pub starting_block_number: u64,
    /// `l1Head()`.
    pub l1_head: B256,
    /// `intermediateOutputRoots()`.
    pub intermediate_output_roots: Vec<B256>,
    /// Address returned by `bondRecipient()`.
    pub bond_recipient: Address,
    /// Value returned by `gameOver()`.
    pub game_over: bool,
    /// Value returned by `bondUnlocked()`.
    pub bond_unlocked: bool,
    /// Value returned by `bondClaimed()`.
    pub bond_claimed: bool,
}

impl MockGameState {
    /// Builds a default `IN_PROGRESS` state with the given prover triple.
    pub const fn in_progress(
        tee_prover: Address,
        zk_prover: Address,
        countered_index: u64,
    ) -> Self {
        Self {
            status: GameStatus::InProgress,
            tee_prover,
            zk_prover,
            countered_index,
            root_claim: B256::ZERO,
            l2_block_number: 0,
            parent_address: Address::ZERO,
            starting_block_number: 0,
            l1_head: B256::ZERO,
            intermediate_output_roots: Vec::new(),
            bond_recipient: Address::ZERO,
            game_over: false,
            bond_unlocked: false,
            bond_claimed: false,
        }
    }
}

/// Helper to create an address from a `u64` index (last 8 bytes).
pub fn addr(index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..20].copy_from_slice(&index.to_be_bytes());
    Address::from(bytes)
}

/// Helper to build a `GameAtIndex` with a deterministic timestamp.
pub fn factory_game(index: u64, game_type: u32) -> GameAtIndex {
    GameAtIndex { game_type, timestamp: 1_000_000 + index, proxy: addr(index) }
}

/// Mock `DisputeGameFactory` backed by an in-memory list of games.
#[derive(Debug, Default)]
pub struct MockDisputeGameFactory {
    /// Games keyed by their factory index.
    pub games: Mutex<Vec<GameAtIndex>>,
    /// Implementation address keyed by `game_type`.
    pub impls: Mutex<HashMap<u32, Address>>,
}

impl MockDisputeGameFactory {
    /// Creates an empty factory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a game to the factory.
    pub fn push(&self, game: GameAtIndex) {
        self.games.lock().expect("games lock poisoned").push(game);
    }

    /// Sets the implementation address for a `game_type`.
    pub fn set_impl(&self, game_type: u32, address: Address) {
        self.impls.lock().expect("impls lock poisoned").insert(game_type, address);
    }
}

#[async_trait]
impl DisputeGameFactoryClient for MockDisputeGameFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        Ok(self.games.lock().expect("games lock poisoned").len() as u64)
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        self.games
            .lock()
            .expect("games lock poisoned")
            .get(index as usize)
            .copied()
            .ok_or_else(|| ContractError::Validation(format!("index {index} out of bounds")))
    }

    async fn game_impls(&self, game_type: u32) -> Result<Address, ContractError> {
        Ok(self
            .impls
            .lock()
            .expect("impls lock poisoned")
            .get(&game_type)
            .copied()
            .unwrap_or(Address::ZERO))
    }

    async fn init_bonds(&self, _game_type: u32) -> Result<U256, ContractError> {
        unimplemented!("init_bonds not used by scanner tests")
    }

    async fn games(
        &self,
        _game_type: u32,
        _root_claim: B256,
        _extra_data: Bytes,
    ) -> Result<Address, ContractError> {
        unimplemented!("games lookup not used by scanner tests")
    }

    async fn find_latest_games(
        &self,
        game_type: u32,
        start: u64,
        n: u64,
    ) -> Result<Vec<(u64, Address)>, ContractError> {
        let games = self.games.lock().expect("games lock poisoned");
        if games.is_empty() || n == 0 {
            return Ok(Vec::new());
        }
        let cap = (games.len() as u64).saturating_sub(1);
        let mut idx = start.min(cap) as i64;
        let mut out = Vec::new();
        while idx >= 0 && (out.len() as u64) < n {
            let entry = games[idx as usize];
            if entry.game_type == game_type {
                out.push((idx as u64, entry.proxy));
            }
            idx -= 1;
        }
        Ok(out)
    }
}

/// Mock `AnchorStateRegistry` with a mutable snapshot.
#[derive(Debug)]
pub struct MockAnchorStateRegistry {
    /// Current anchor snapshot.
    pub snapshot: Mutex<AnchorSnapshot>,
    /// When `true`, `anchor_snapshot()` returns a [`ContractError::Validation`].
    pub fail_snapshot: Mutex<bool>,
}

impl MockAnchorStateRegistry {
    /// Creates a registry whose snapshot points at `anchor_game`.
    pub const fn new(anchor_game: Address) -> Self {
        Self {
            snapshot: Mutex::new(AnchorSnapshot {
                anchor_root: AnchorRoot { root: B256::ZERO, l2_block_number: 0 },
                anchor_game,
            }),
            fail_snapshot: Mutex::new(false),
        }
    }

    /// Replaces the anchor game.
    pub fn set_anchor_game(&self, anchor_game: Address) {
        self.snapshot.lock().expect("snapshot lock poisoned").anchor_game = anchor_game;
    }

    /// Toggles whether `anchor_snapshot()` should return an error.
    pub fn set_fail_snapshot(&self, fail: bool) {
        *self.fail_snapshot.lock().expect("fail_snapshot lock poisoned") = fail;
    }
}

#[async_trait]
impl AnchorStateRegistryClient for MockAnchorStateRegistry {
    async fn anchor_snapshot(&self) -> Result<AnchorSnapshot, ContractError> {
        if *self.fail_snapshot.lock().expect("fail_snapshot lock poisoned") {
            return Err(ContractError::Validation("simulated anchor snapshot error".into()));
        }
        Ok(*self.snapshot.lock().expect("snapshot lock poisoned"))
    }
}

/// Mock `AggregateVerifier` with per-game state.
#[derive(Debug, Default)]
pub struct MockAggregateVerifier {
    /// Game state keyed by proxy address.
    pub games: Mutex<HashMap<Address, MockGameState>>,
    /// Value returned by `read_intermediate_block_interval`.
    pub intermediate_block_interval: Mutex<u64>,
}

impl MockAggregateVerifier {
    /// Creates a verifier with the default interval (`5`).
    pub fn new() -> Self {
        Self { games: Mutex::new(HashMap::new()), intermediate_block_interval: Mutex::new(5) }
    }

    /// Inserts (or replaces) the state for a game.
    pub fn set_game(&self, address: Address, state: MockGameState) {
        self.games.lock().expect("games lock poisoned").insert(address, state);
    }

    /// Replaces the global `intermediate_block_interval`.
    pub fn set_interval(&self, interval: u64) {
        *self.intermediate_block_interval.lock().expect("interval lock poisoned") = interval;
    }

    fn get<T>(
        &self,
        address: Address,
        f: impl FnOnce(&MockGameState) -> T,
    ) -> Result<T, ContractError> {
        self.games
            .lock()
            .expect("games lock poisoned")
            .get(&address)
            .map(f)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {address}")))
    }
}

#[async_trait]
impl AggregateVerifierClient for MockAggregateVerifier {
    async fn game_info(&self, address: Address) -> Result<VerifierGameInfo, ContractError> {
        self.get(address, |s| VerifierGameInfo {
            root_claim: s.root_claim,
            l2_block_number: s.l2_block_number,
            parent_address: s.parent_address,
        })
    }

    async fn status(&self, address: Address) -> Result<GameStatus, ContractError> {
        self.get(address, |s| s.status)
    }

    async fn zk_prover(&self, address: Address) -> Result<Address, ContractError> {
        self.get(address, |s| s.zk_prover)
    }

    async fn tee_prover(&self, address: Address) -> Result<Address, ContractError> {
        self.get(address, |s| s.tee_prover)
    }

    async fn starting_block_number(&self, address: Address) -> Result<u64, ContractError> {
        self.get(address, |s| s.starting_block_number)
    }

    async fn l1_head(&self, address: Address) -> Result<B256, ContractError> {
        self.get(address, |s| s.l1_head)
    }

    async fn read_block_interval(&self, _impl_address: Address) -> Result<u64, ContractError> {
        unimplemented!("read_block_interval not used by scanner tests")
    }

    async fn read_intermediate_block_interval(
        &self,
        _impl_address: Address,
    ) -> Result<u64, ContractError> {
        Ok(*self.intermediate_block_interval.lock().expect("interval lock poisoned"))
    }

    async fn intermediate_output_roots(
        &self,
        address: Address,
    ) -> Result<Vec<B256>, ContractError> {
        self.get(address, |s| s.intermediate_output_roots.clone())
    }

    async fn intermediate_output_root(
        &self,
        _address: Address,
        _index: u64,
    ) -> Result<B256, ContractError> {
        unimplemented!("intermediate_output_root not used by scanner tests")
    }

    async fn countered_index(&self, address: Address) -> Result<u64, ContractError> {
        self.get(address, |s| s.countered_index)
    }

    async fn game_over(&self, address: Address) -> Result<bool, ContractError> {
        self.get(address, |s| s.game_over)
    }

    async fn resolved_at(&self, _address: Address) -> Result<u64, ContractError> {
        unimplemented!("resolved_at not used by scanner tests")
    }

    async fn bond_recipient(&self, address: Address) -> Result<Address, ContractError> {
        self.get(address, |s| s.bond_recipient)
    }

    async fn bond_unlocked(&self, address: Address) -> Result<bool, ContractError> {
        self.get(address, |s| s.bond_unlocked)
    }

    async fn bond_claimed(&self, address: Address) -> Result<bool, ContractError> {
        self.get(address, |s| s.bond_claimed)
    }

    async fn expected_resolution(&self, _address: Address) -> Result<u64, ContractError> {
        unimplemented!("expected_resolution not used by scanner tests")
    }

    async fn proof_count(&self, _address: Address) -> Result<u8, ContractError> {
        unimplemented!("proof_count not used by scanner tests")
    }

    async fn created_at(&self, _address: Address) -> Result<u64, ContractError> {
        unimplemented!("created_at not used by scanner tests")
    }

    async fn delayed_weth(&self, _address: Address) -> Result<Address, ContractError> {
        unimplemented!("delayed_weth not used by scanner tests")
    }

    async fn anchor_state_registry(&self, _address: Address) -> Result<Address, ContractError> {
        unimplemented!("anchor_state_registry not used by scanner tests")
    }

    async fn is_game_finalized(
        &self,
        _asr: Address,
        _address: Address,
    ) -> Result<bool, ContractError> {
        unimplemented!("is_game_finalized not used by scanner tests")
    }

    async fn anchor_preflight(
        &self,
        _asr: Address,
        _address: Address,
    ) -> Result<AnchorPreflight, ContractError> {
        unimplemented!("anchor_preflight not used by scanner tests")
    }
}

/// Mock [`DelayedWETHClient`] backed by an in-memory map.
///
/// Programmed via [`MockDelayedWETH::set_withdrawal`]. Unset
/// `(holder, recipient)` pairs return `(0, 0)`, mirroring the Solidity
/// mapping default.
#[derive(Debug)]
pub struct MockDelayedWETH {
    /// Withdrawal delay returned by [`Self::delay`].
    pub delay: Mutex<Duration>,
    /// Recorded `(amount, timestamp)` keyed by `(holder, recipient)`.
    pub withdrawals: Mutex<HashMap<(Address, Address), (U256, u64)>>,
}

impl MockDelayedWETH {
    /// Creates a mock with the given withdrawal delay and no recorded withdrawals.
    pub fn new(delay: Duration) -> Self {
        Self { delay: Mutex::new(delay), withdrawals: Mutex::new(HashMap::new()) }
    }

    /// Replaces the withdrawal delay returned by [`Self::delay`].
    pub fn set_delay(&self, delay: Duration) {
        *self.delay.lock().expect("delay lock poisoned") = delay;
    }

    /// Records the `(amount, timestamp)` pair for the `(holder, recipient)` key.
    pub fn set_withdrawal(
        &self,
        holder: Address,
        recipient: Address,
        amount: U256,
        timestamp: u64,
    ) {
        self.withdrawals
            .lock()
            .expect("withdrawals lock poisoned")
            .insert((holder, recipient), (amount, timestamp));
    }
}

#[async_trait]
impl DelayedWETHClient for MockDelayedWETH {
    async fn delay(&self) -> Result<Duration, ContractError> {
        Ok(*self.delay.lock().expect("delay lock poisoned"))
    }

    async fn withdrawals(
        &self,
        holder: Address,
        recipient: Address,
    ) -> Result<(U256, u64), ContractError> {
        Ok(self
            .withdrawals
            .lock()
            .expect("withdrawals lock poisoned")
            .get(&(holder, recipient))
            .copied()
            .unwrap_or((U256::ZERO, 0)))
    }
}

/// Mock [`DelayedWETHResolver`] returning the same [`MockDelayedWETH`]
/// for every game address.
#[derive(Debug)]
pub struct MockDelayedWETHResolver {
    inner: Arc<MockDelayedWETH>,
}

impl MockDelayedWETHResolver {
    /// Wraps `inner` so every `resolve` call returns it.
    pub const fn new(inner: Arc<MockDelayedWETH>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl DelayedWETHResolver for MockDelayedWETHResolver {
    async fn resolve(&self, _game: Address) -> Result<Arc<dyn DelayedWETHClient>, ContractError> {
        Ok(Arc::<MockDelayedWETH>::clone(&self.inner))
    }
}

/// Mock [`ZkProofProvider`] backed by FIFO response queues.
///
/// Tests enqueue responses with the `push_*` helpers; each `prove_block`
/// or `get_proof` call pops the next response. Calls are recorded so
/// tests can assert on session id stability across retries.
#[derive(Debug, Default)]
pub struct MockZkProofProvider {
    prove_responses: Mutex<VecDeque<Result<ProveBlockResponse, ZkProofError>>>,
    get_responses: Mutex<VecDeque<Result<GetProofResponse, ZkProofError>>>,
    prove_calls: Mutex<Vec<ProveBlockRequest>>,
    get_calls: Mutex<Vec<GetProofRequest>>,
}

impl MockZkProofProvider {
    /// Returns a new provider with empty queues.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues a successful `prove_block` response. The returned
    /// session id echoes back whatever the caller already supplied.
    pub fn push_prove_ok(&self) {
        self.prove_responses
            .lock()
            .expect("prove_responses lock poisoned")
            .push_back(Ok(ProveBlockResponse { session_id: String::new() }));
    }

    /// Enqueues a `get_proof` response with status `Succeeded` and
    /// the given receipt bytes.
    pub fn push_get_succeeded(&self, receipt: Vec<u8>) {
        self.get_responses.lock().expect("get_responses lock poisoned").push_back(Ok(
            GetProofResponse {
                status: ProofJobStatus::Succeeded as i32,
                receipt,
                error_message: None,
            },
        ));
    }

    /// Enqueues a `get_proof` response with status `Failed` and the
    /// given error message.
    pub fn push_get_failed(&self, error_message: Option<String>) {
        self.get_responses.lock().expect("get_responses lock poisoned").push_back(Ok(
            GetProofResponse {
                status: ProofJobStatus::Failed as i32,
                receipt: Vec::new(),
                error_message,
            },
        ));
    }

    /// Enqueues a `get_proof` response with status `Pending` so the
    /// caller's poll loop keeps spinning.
    pub fn push_get_pending(&self) {
        self.get_responses.lock().expect("get_responses lock poisoned").push_back(Ok(
            GetProofResponse {
                status: ProofJobStatus::Pending as i32,
                receipt: Vec::new(),
                error_message: None,
            },
        ));
    }

    /// Snapshot of every `prove_block` request observed so far.
    pub fn prove_calls(&self) -> Vec<ProveBlockRequest> {
        self.prove_calls.lock().expect("prove_calls lock poisoned").clone()
    }

    /// Snapshot of every `get_proof` request observed so far.
    pub fn get_calls(&self) -> Vec<GetProofRequest> {
        self.get_calls.lock().expect("get_calls lock poisoned").clone()
    }
}

#[async_trait]
impl ZkProofProvider for MockZkProofProvider {
    async fn prove_block(
        &self,
        request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        self.prove_calls.lock().expect("prove_calls lock poisoned").push(request);
        self.prove_responses
            .lock()
            .expect("prove_responses lock poisoned")
            .pop_front()
            .expect("MockZkProofProvider: prove_block called without an enqueued response")
    }

    async fn get_proof(&self, request: GetProofRequest) -> Result<GetProofResponse, ZkProofError> {
        self.get_calls.lock().expect("get_calls lock poisoned").push(request);
        self.get_responses
            .lock()
            .expect("get_responses lock poisoned")
            .pop_front()
            .expect("MockZkProofProvider: get_proof called without an enqueued response")
    }
}

/// Recorded `prove_range` call for inspection by tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeProveRangeCall {
    /// L2 block at the start of the proven range.
    pub start_block: u64,
    /// Output root at `start_block`.
    pub start_root: B256,
    /// L2 block at the end of the proven range.
    pub end_block: u64,
    /// Output root the caller expects the enclave to sign at `end_block`.
    pub end_root: B256,
    /// L1 head hash provided to the TEE.
    pub l1_head: B256,
    /// Checkpoint interval forwarded to the TEE.
    pub intermediate_block_interval: u64,
}

/// Mock [`TeeProofProvider`] backed by a FIFO response queue.
#[derive(Debug, Default)]
pub struct MockTeeProofProvider {
    responses: Mutex<VecDeque<Result<TeeProofResult, TeeProofError>>>,
    calls: Mutex<Vec<TeeProveRangeCall>>,
}

impl MockTeeProofProvider {
    /// Returns a new provider with an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues a successful response with the given root and signature bytes.
    pub fn push_ok(&self, signed_root: B256, signature_bytes: Bytes) {
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .push_back(Ok(TeeProofResult { signed_root, signature_bytes }));
    }

    /// Enqueues a failure response.
    pub fn push_err(&self, err: TeeProofError) {
        self.responses.lock().expect("responses lock poisoned").push_back(Err(err));
    }

    /// Snapshot of every `prove_range` call observed so far.
    pub fn calls(&self) -> Vec<TeeProveRangeCall> {
        self.calls.lock().expect("calls lock poisoned").clone()
    }
}

#[async_trait]
impl TeeProofProvider for MockTeeProofProvider {
    async fn prove_range(
        &self,
        start_block: u64,
        start_root: B256,
        end_block: u64,
        end_root: B256,
        l1_head: B256,
        intermediate_block_interval: u64,
    ) -> Result<TeeProofResult, TeeProofError> {
        self.calls.lock().expect("calls lock poisoned").push(TeeProveRangeCall {
            start_block,
            start_root,
            end_block,
            end_root,
            l1_head,
            intermediate_block_interval,
        });
        self.responses
            .lock()
            .expect("responses lock poisoned")
            .pop_front()
            .expect("MockTeeProofProvider: prove_range called without an enqueued response")
    }
}

/// Builds a minimal [`TransactionReceipt`] with the given execution
/// status and transaction hash. Used by [`MockTxManager`] to feed
/// success/revert receipts to a [`crate::SubmissionTask`] under test.
pub const fn receipt_with_status(success: bool, tx_hash: B256) -> TransactionReceipt {
    let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
        receipt: Receipt {
            status: Eip658Value::Eip658(success),
            cumulative_gas_used: 21_000,
            logs: Vec::new(),
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

/// Mock [`TxManager`] backed by a FIFO response queue.
///
/// Cheaply [`Clone`]d so a test can keep one handle to inspect calls
/// while another is moved into a spawned [`crate::SubmissionTask`];
/// both views share the same internal state through an [`Arc`].
#[derive(Debug, Clone)]
pub struct MockTxManager {
    state: Arc<MockTxManagerState>,
    sender_address: Address,
}

#[derive(Debug, Default)]
struct MockTxManagerState {
    responses: Mutex<VecDeque<SendResponse>>,
    calls: Mutex<Vec<TxCandidate>>,
}

impl MockTxManager {
    /// Creates a new mock that reports `sender_address` as its sender.
    pub fn new(sender_address: Address) -> Self {
        Self { state: Arc::new(MockTxManagerState::default()), sender_address }
    }

    /// Enqueues a successful receipt with `success == true`.
    pub fn push_success(&self, tx_hash: B256) {
        self.push_response(Ok(receipt_with_status(true, tx_hash)));
    }

    /// Enqueues a confirmed receipt that reverted on-chain
    /// (`success == false`).
    pub fn push_revert(&self, tx_hash: B256) {
        self.push_response(Ok(receipt_with_status(false, tx_hash)));
    }

    /// Enqueues an error response (`send` returns `Err(error)`).
    pub fn push_error(&self, error: base_tx_manager::TxManagerError) {
        self.push_response(Err(error));
    }

    /// Snapshot of every [`TxCandidate`] passed to `send` so far.
    pub fn calls(&self) -> Vec<TxCandidate> {
        self.state.calls.lock().expect("calls lock poisoned").clone()
    }

    fn push_response(&self, response: SendResponse) {
        self.state.responses.lock().expect("responses lock poisoned").push_back(response);
    }
}

impl TxManager for MockTxManager {
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        self.state.calls.lock().expect("calls lock poisoned").push(candidate);
        self.state
            .responses
            .lock()
            .expect("responses lock poisoned")
            .pop_front()
            .expect("MockTxManager: send called without an enqueued response")
    }

    async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
        unimplemented!("send_async is not exercised by SubmissionTask tests")
    }

    fn sender_address(&self) -> Address {
        self.sender_address
    }
}
