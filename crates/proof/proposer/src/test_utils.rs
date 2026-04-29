//! Shared test utilities: reusable mock stubs for L1/L2 clients, contract clients, and proposer.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorPreflight, AnchorRoot, AnchorStateRegistryClient, ContractError,
    DisputeGameFactoryClient, GameAtIndex, GameInfo,
};
use base_proof_primitives::{ProofResult, Proposal, ProverClient};
use base_proof_rpc::{
    BaseBlock, L1BlockId, L1BlockRef, L1Provider, L2BlockRef, L2Provider, OutputAtBlock,
    RollupProvider, RpcError, RpcResult, SyncStatus,
};

use crate::{error::ProposerError, output_proposer::OutputProposer};

/// Mock L1 provider for tests.
#[derive(Debug)]
pub struct MockL1 {
    /// The block number returned by `block_number()`.
    pub latest_block_number: u64,
}

#[async_trait]
impl L1Provider for MockL1 {
    async fn block_number(&self) -> RpcResult<u64> {
        Ok(self.latest_block_number)
    }
    async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
        Ok(alloy_rpc_types_eth::Header { hash: B256::repeat_byte(0x11), ..Default::default() })
    }
    async fn header_by_hash(&self, _: B256) -> RpcResult<alloy_rpc_types_eth::Header> {
        unimplemented!()
    }
    async fn block_receipts(
        &self,
        _: B256,
    ) -> RpcResult<Vec<alloy_rpc_types_eth::TransactionReceipt>> {
        unimplemented!()
    }
    async fn code_at(&self, _: Address, _: Option<u64>) -> RpcResult<Bytes> {
        unimplemented!()
    }
    async fn call_contract(&self, _: Address, _: Bytes, _: Option<u64>) -> RpcResult<Bytes> {
        unimplemented!()
    }
    async fn get_balance(&self, _: Address) -> RpcResult<U256> {
        Ok(U256::ZERO)
    }
}

/// Mock L2 provider for tests.
#[derive(Debug)]
pub struct MockL2 {
    /// When true, `block_by_number` returns a `BlockNotFound` error.
    pub block_not_found: bool,
    /// If set, `header_by_number` returns a header with this hash.
    /// Used for reorg detection tests.
    pub canonical_hash: Option<B256>,
}

#[async_trait]
impl L2Provider for MockL2 {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        unimplemented!()
    }
    async fn get_proof(&self, _: Address, _: B256) -> RpcResult<EIP1186AccountProofResponse> {
        unimplemented!()
    }
    async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
        let hash = self.canonical_hash.unwrap_or(B256::repeat_byte(0x30));
        Ok(alloy_rpc_types_eth::Header { hash, ..Default::default() })
    }
    async fn block_by_number(&self, _: Option<u64>) -> RpcResult<BaseBlock> {
        if self.block_not_found {
            Err(RpcError::BlockNotFound("mock: no blocks".into()))
        } else {
            unimplemented!()
        }
    }
    async fn block_by_hash(&self, _: B256) -> RpcResult<BaseBlock> {
        unimplemented!()
    }
}

/// Mock rollup node client for tests.
///
/// When `max_safe_block` is set, `output_at_block` returns an error for any
/// block number exceeding the limit, simulating a rollup node that hasn't
/// reached that safe head yet.
#[derive(Debug)]
pub struct MockRollupClient {
    /// The sync status returned by `sync_status()`.
    pub sync_status: SyncStatus,
    /// Map of block number to output root returned by `output_at_block()`.
    pub output_roots: HashMap<u64, B256>,
    /// When set, blocks beyond this number return an error.
    pub max_safe_block: Option<u64>,
}

#[async_trait]
impl RollupProvider for MockRollupClient {
    async fn rollup_config(&self) -> RpcResult<RollupConfig> {
        unimplemented!()
    }
    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        Ok(self.sync_status.clone())
    }
    async fn output_at_block(&self, block_number: u64) -> RpcResult<OutputAtBlock> {
        if let Some(max) = self.max_safe_block
            && block_number > max
        {
            return Err(RpcError::BlockNotFound(format!(
                "mock: block {block_number} beyond safe head {max}"
            )));
        }
        let root = self
            .output_roots
            .get(&block_number)
            .copied()
            .unwrap_or_else(|| B256::repeat_byte(block_number as u8));
        Ok(OutputAtBlock { output_root: root, block_ref: test_l2_block_ref(block_number, root) })
    }
}

/// Mock anchor state registry contract client for tests.
#[derive(Debug)]
pub struct MockAnchorStateRegistry {
    /// The anchor root returned by `get_anchor_root()`.
    pub anchor_root: AnchorRoot,
}

#[async_trait]
impl AnchorStateRegistryClient for MockAnchorStateRegistry {
    async fn get_anchor_root(&self) -> Result<AnchorRoot, ContractError> {
        Ok(self.anchor_root)
    }
}

/// Mock dispute game factory contract client for tests.
///
/// `uuid_games` stores games keyed by `(game_type, root_claim, extra_data)` for
/// the `games()` UUID-based lookup. When a key is not found, the lookup returns
/// `Address::ZERO` (no game exists).
///
/// When `games_should_fail` is `true`, all `games()` calls return a
/// `ContractError::Validation` to simulate RPC failures.
#[derive(Debug)]
pub struct MockDisputeGameFactory {
    /// The list of games returned by `game_at_index()`.
    pub games: Vec<GameAtIndex>,
    /// If set, overrides the game count returned by `game_count()`.
    pub game_count_override: Option<u64>,
    /// UUID-keyed game proxy lookups for `games()`.
    pub uuid_games: HashMap<(u32, B256, Bytes), Address>,
    /// When true, all `games()` calls return an error.
    pub games_should_fail: bool,
}

impl MockDisputeGameFactory {
    /// Creates a new mock with the given games and no count override.
    pub fn with_games(games: Vec<GameAtIndex>) -> Self {
        Self {
            games,
            game_count_override: None,
            uuid_games: HashMap::new(),
            games_should_fail: false,
        }
    }
}

#[async_trait]
impl DisputeGameFactoryClient for MockDisputeGameFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        Ok(self.game_count_override.unwrap_or(self.games.len() as u64))
    }
    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        self.games
            .get(index as usize)
            .copied()
            .ok_or_else(|| ContractError::Validation(format!("index {index} out of bounds")))
    }
    async fn init_bonds(&self, _: u32) -> Result<U256, ContractError> {
        Ok(U256::ZERO)
    }
    async fn game_impls(&self, _: u32) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn games(
        &self,
        game_type: u32,
        root_claim: B256,
        extra_data: Bytes,
    ) -> Result<Address, ContractError> {
        if self.games_should_fail {
            return Err(ContractError::Validation("mock: simulated games() RPC failure".into()));
        }
        let key = (game_type, root_claim, extra_data);
        Ok(self.uuid_games.get(&key).copied().unwrap_or(Address::ZERO))
    }
}

/// Mock aggregate verifier contract client for tests.
#[derive(Debug, Default)]
pub struct MockAggregateVerifier {
    /// Map of game address to game info returned by `game_info()`.
    pub game_info_map: HashMap<Address, GameInfo>,
    /// Addresses for which `game_info()` returns an error.
    pub failing_addresses: HashSet<Address>,
    /// Map of game address to intermediate output roots.
    pub intermediate_roots_map: HashMap<Address, Vec<B256>>,
}

impl MockAggregateVerifier {
    /// Creates a new mock with the given game info map.
    pub fn with_game_info(map: HashMap<Address, GameInfo>) -> Self {
        Self { game_info_map: map, ..Default::default() }
    }
}

#[async_trait]
impl AggregateVerifierClient for MockAggregateVerifier {
    async fn game_info(&self, addr: Address) -> Result<GameInfo, ContractError> {
        if self.failing_addresses.contains(&addr) {
            return Err(ContractError::Validation(format!(
                "mock: simulated game_info failure for {addr}"
            )));
        }
        Ok(self.game_info_map.get(&addr).copied().unwrap_or(GameInfo {
            root_claim: B256::ZERO,
            l2_block_number: 0,
            parent_address: Address::ZERO,
        }))
    }
    async fn status(&self, _: Address) -> Result<u8, ContractError> {
        Ok(0)
    }
    async fn zk_prover(&self, _: Address) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn tee_prover(&self, _: Address) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn starting_block_number(&self, _: Address) -> Result<u64, ContractError> {
        Ok(0)
    }
    async fn l1_head(&self, _: Address) -> Result<B256, ContractError> {
        Ok(B256::ZERO)
    }
    async fn read_block_interval(&self, _: Address) -> Result<u64, ContractError> {
        Ok(512)
    }
    async fn read_intermediate_block_interval(&self, _: Address) -> Result<u64, ContractError> {
        Ok(512)
    }
    async fn intermediate_output_roots(&self, addr: Address) -> Result<Vec<B256>, ContractError> {
        if let Some(roots) = self.intermediate_roots_map.get(&addr) {
            return Ok(roots.clone());
        }
        if let Some(info) = self.game_info_map.get(&addr) {
            return Ok(vec![info.root_claim]);
        }
        Ok(vec![B256::ZERO])
    }
    async fn intermediate_output_root(
        &self,
        addr: Address,
        index: u64,
    ) -> Result<B256, ContractError> {
        let roots = self.intermediate_output_roots(addr).await?;
        Ok(roots
            .get(index as usize)
            .copied()
            .expect("intermediate_output_root: index out of bounds"))
    }
    async fn countered_index(&self, _: Address) -> Result<u64, ContractError> {
        Ok(0)
    }
    async fn game_over(&self, _: Address) -> Result<bool, ContractError> {
        Ok(false)
    }
    async fn resolved_at(&self, _: Address) -> Result<u64, ContractError> {
        Ok(0)
    }
    async fn bond_recipient(&self, _: Address) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn bond_unlocked(&self, _: Address) -> Result<bool, ContractError> {
        Ok(false)
    }
    async fn bond_claimed(&self, _: Address) -> Result<bool, ContractError> {
        Ok(false)
    }
    async fn expected_resolution(&self, _: Address) -> Result<u64, ContractError> {
        Ok(0)
    }
    async fn proof_count(&self, _: Address) -> Result<u8, ContractError> {
        Ok(0)
    }
    async fn created_at(&self, _: Address) -> Result<u64, ContractError> {
        Ok(0)
    }
    async fn delayed_weth(&self, _: Address) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn anchor_state_registry(&self, _: Address) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn is_game_finalized(&self, _: Address, _: Address) -> Result<bool, ContractError> {
        Ok(true)
    }
    async fn anchor_preflight(
        &self,
        _: Address,
        _: Address,
    ) -> Result<AnchorPreflight, ContractError> {
        Ok(AnchorPreflight {
            blacklisted: false,
            retired: false,
            respected: true,
            anchor_root: AnchorRoot { root: B256::ZERO, l2_block_number: 0 },
        })
    }
}

/// Creates a test [`L1BlockRef`] with the given block number.
pub fn test_l1_block_ref(number: u64) -> L1BlockRef {
    L1BlockRef { hash: B256::ZERO, number, parent_hash: B256::ZERO, timestamp: 1_000_000 + number }
}

/// Creates a test [`L2BlockRef`] with the given block number and hash.
pub fn test_l2_block_ref(number: u64, hash: B256) -> L2BlockRef {
    L2BlockRef {
        hash,
        number,
        parent_hash: B256::ZERO,
        timestamp: 1_000_000 + number,
        l1origin: L1BlockId { hash: B256::ZERO, number: 100 + number },
        sequence_number: 0,
    }
}

/// Creates a test [`SyncStatus`] with the given safe block number and hash.
pub fn test_sync_status(safe_number: u64, safe_hash: B256) -> SyncStatus {
    let l1 = test_l1_block_ref(100);
    let l2 = test_l2_block_ref(safe_number, safe_hash);
    SyncStatus {
        current_l1: l1,
        current_l1_finalized: None,
        head_l1: l1,
        safe_l1: l1,
        finalized_l1: l1,
        unsafe_l2: l2,
        safe_l2: l2,
        finalized_l2: l2,
        pending_safe_l2: None,
    }
}

/// Creates a test [`AnchorRoot`] with the given L2 block number.
pub fn test_anchor_root(block_number: u64) -> AnchorRoot {
    AnchorRoot { root: B256::ZERO, l2_block_number: block_number }
}

/// Creates a test [`Proposal`] with the given L2 block number.
pub fn test_proposal(block_number: u64) -> Proposal {
    Proposal {
        output_root: B256::repeat_byte(block_number as u8),
        signature: Bytes::from_static(&[0xab; 65]),
        l1_origin_hash: B256::repeat_byte(0x02),
        l1_origin_number: 100 + block_number,
        l2_block_number: block_number,
        prev_output_root: B256::repeat_byte(0x03),
        config_hash: B256::repeat_byte(0x04),
    }
}

/// Mock prover client for tests.
#[derive(Debug)]
pub struct MockProver {
    /// Simulated proving delay.
    pub delay: Duration,
    /// Block interval used to generate intermediate proposals.
    pub block_interval: u64,
}

#[async_trait]
impl ProverClient for MockProver {
    async fn prove(
        &self,
        request: base_proof_primitives::ProofRequest,
    ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
        tokio::time::sleep(self.delay).await;

        let block_number = request.claimed_l2_block_number;

        let start = block_number.saturating_sub(self.block_interval);
        let proposals: Vec<Proposal> = ((start + 1)..=block_number).map(test_proposal).collect();

        Ok(ProofResult::Tee { aggregate_proposal: test_proposal(block_number), proposals })
    }
}

/// Mock output proposer that always succeeds.
#[derive(Debug)]
pub struct MockOutputProposer;

#[async_trait]
impl OutputProposer for MockOutputProposer {
    async fn propose_output(
        &self,
        _proposal: &Proposal,
        _parent_address: Address,
        _intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        Ok(())
    }
}
