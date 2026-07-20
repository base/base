//! Shared test utilities: reusable mock stubs for L1/L2 clients, contract clients, and proposer.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_optimism_rpc::{L1BlockId, L1BlockRef, L2BlockRef, OutputAtBlock, SyncStatus};
use base_proof_contracts::{
    AggregateVerifierClient, AnchorPreflight, AnchorRoot, AnchorSnapshot,
    AnchorStateRegistryClient, ContractError, DisputeGameFactoryClient, GameAtIndex, GameInfo,
    GameStatus,
};
use base_proof_primitives::Proposal;
use base_proof_rpc::{
    BaseBlock, BaseHeader, L1Provider, L2Provider, RollupProvider, RpcError, RpcResult,
};
use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
use base_prover_service_protocol::{
    DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    PROOF_REQUEST_NOT_FOUND_MESSAGE, ProofRequestIdCollisionMessage,
    ProofRequestKind as ApiProofRequestKind, ProofResult as ApiProofResult, ProofStatus,
    ProveBlockRangeRequest, ProveBlockRangeResponse, TeeKind, TeeProofResult,
};
use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorObjectOwned};

use crate::{error::ProposerError, output_proposer::OutputProposer};

const TEST_SIGNATURE: [u8; 65] = {
    let mut signature = [0xab; 65];
    signature[64] = 1;
    signature
};

/// Mock L1 provider for tests.
#[derive(Debug, Default)]
pub struct MockL1 {
    /// The block number returned by `block_number()`.
    pub latest_block_number: u64,
    /// Headers returned by `header_by_hash()`.
    pub headers_by_hash: HashMap<B256, Header>,
}

impl MockL1 {
    /// Creates a mock L1 provider with the default test L1 head registered.
    pub fn new(latest_block_number: u64) -> Self {
        Self {
            latest_block_number,
            headers_by_hash: [(B256::ZERO, test_l1_header(B256::ZERO, latest_block_number))].into(),
        }
    }
}

#[async_trait]
impl L1Provider for MockL1 {
    async fn block_number(&self) -> RpcResult<u64> {
        Ok(self.latest_block_number)
    }
    async fn header_by_number(
        &self,
        _: BlockNumberOrTag,
    ) -> RpcResult<alloy_rpc_types_eth::Header> {
        Ok(test_l1_header(B256::repeat_byte(0x11), self.latest_block_number))
    }
    async fn header_by_hash(&self, hash: B256) -> RpcResult<alloy_rpc_types_eth::Header> {
        self.headers_by_hash
            .get(&hash)
            .cloned()
            .ok_or_else(|| RpcError::HeaderNotFound(format!("mock: no header for hash {hash}")))
    }
    async fn block_receipts(
        &self,
        _: B256,
    ) -> RpcResult<Vec<alloy_rpc_types_eth::TransactionReceipt>> {
        unimplemented!()
    }
    async fn code_at(&self, _: Address, _: BlockNumberOrTag) -> RpcResult<Bytes> {
        unimplemented!()
    }
    async fn call_contract(&self, _: Address, _: Bytes, _: BlockNumberOrTag) -> RpcResult<Bytes> {
        unimplemented!()
    }
    async fn get_balance(&self, _: Address) -> RpcResult<U256> {
        Ok(U256::ZERO)
    }
}

/// Mock L2 provider for tests.
#[derive(Debug)]
pub struct MockL2;

#[async_trait]
impl L2Provider for MockL2 {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        unimplemented!()
    }
    async fn get_proof(&self, _: Address, _: B256) -> RpcResult<EIP1186AccountProofResponse> {
        unimplemented!()
    }
    async fn header_by_number(&self, _: BlockNumberOrTag) -> RpcResult<BaseHeader> {
        Ok(Header::<alloy_consensus::Header> {
            hash: B256::repeat_byte(0x30),
            ..Default::default()
        }
        .into())
    }
    async fn block_by_number(&self, _: BlockNumberOrTag) -> RpcResult<BaseBlock> {
        unimplemented!()
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
    async fn fresh_output_at_block(&self, block_number: u64) -> RpcResult<OutputAtBlock> {
        self.output_at_block(block_number).await
    }
}

/// Mock anchor state registry contract client for tests.
#[derive(Debug)]
pub struct MockAnchorStateRegistry {
    /// The anchor root returned by `anchor_snapshot()`.
    pub anchor_root: AnchorRoot,
    /// The anchor game returned by `anchor_snapshot()`.
    pub anchor_game: Address,
}

#[async_trait]
impl AnchorStateRegistryClient for MockAnchorStateRegistry {
    async fn anchor_snapshot(&self) -> Result<AnchorSnapshot, ContractError> {
        Ok(AnchorSnapshot { anchor_root: self.anchor_root, anchor_game: self.anchor_game })
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
#[derive(Debug, Default)]
pub struct MockDisputeGameFactory {
    /// The game count returned by `game_count()`.
    pub game_count: u64,
    /// UUID-keyed game proxy lookups for `games()`.
    pub uuid_games: HashMap<(u32, B256, Bytes), Address>,
    /// Ordered responses for repeated `games()` calls.
    pub uuid_game_responses: Mutex<VecDeque<Address>>,
    /// When true, all `games()` calls return an error.
    pub games_should_fail: bool,
}

impl MockDisputeGameFactory {
    /// Creates a mock whose `games()` calls pop from the given responses.
    pub fn with_uuid_game_responses(responses: impl IntoIterator<Item = Address>) -> Self {
        Self { uuid_game_responses: Mutex::new(responses.into_iter().collect()), ..Self::default() }
    }
}

#[async_trait]
impl DisputeGameFactoryClient for MockDisputeGameFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        Ok(self.game_count)
    }
    async fn game_at_index(&self, _: u64) -> Result<GameAtIndex, ContractError> {
        unimplemented!("unused in proposer tests")
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
        if let Some(response) = self.uuid_game_responses.lock().unwrap().pop_front() {
            return Ok(response);
        }
        Ok(self
            .uuid_games
            .get(&(game_type, root_claim, extra_data))
            .copied()
            .unwrap_or(Address::ZERO))
    }
}

/// Mock aggregate verifier contract client for tests.
#[derive(Debug, Default)]
pub struct MockAggregateVerifier {
    /// L1 head returned by `l1_head()`.
    pub l1_head: B256,
}

#[async_trait]
impl AggregateVerifierClient for MockAggregateVerifier {
    async fn game_info(&self, _: Address) -> Result<GameInfo, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn status(&self, _: Address) -> Result<GameStatus, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn zk_prover(&self, _: Address) -> Result<Address, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn tee_prover(&self, _: Address) -> Result<Address, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn starting_block_number(&self, _: Address) -> Result<u64, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn l1_head(&self, _: Address) -> Result<B256, ContractError> {
        Ok(self.l1_head)
    }
    async fn read_block_interval(&self, _: Address) -> Result<u64, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn read_intermediate_block_interval(&self, _: Address) -> Result<u64, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn intermediate_output_roots(&self, _: Address) -> Result<Vec<B256>, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn intermediate_output_root(&self, _: Address, _: u64) -> Result<B256, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn countered_index(&self, _: Address) -> Result<u64, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn game_over(&self, _: Address) -> Result<bool, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn resolved_at(&self, _: Address) -> Result<u64, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn bond_recipient(&self, _: Address) -> Result<Address, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn bond_unlocked(&self, _: Address) -> Result<bool, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn bond_claimed(&self, _: Address) -> Result<bool, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn expected_resolution(&self, _: Address) -> Result<u64, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn proof_count(&self, _: Address) -> Result<u8, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn created_at(&self, _: Address) -> Result<u64, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn delayed_weth(&self, _: Address) -> Result<Address, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn anchor_state_registry(&self, _: Address) -> Result<Address, ContractError> {
        unimplemented!("unused in proposer tests")
    }
    async fn is_game_finalized(&self, _: Address, _: Address) -> Result<bool, ContractError> {
        unimplemented!("unused in proposer tests")
    }

    async fn anchor_preflight(
        &self,
        _: Address,
        _: Address,
    ) -> Result<AnchorPreflight, ContractError> {
        unimplemented!("unused in proposer tests")
    }
}

/// Creates a test L1 RPC header with the given block hash and number.
pub fn test_l1_header(hash: B256, number: u64) -> Header {
    Header {
        hash,
        inner: alloy_consensus::Header { number, ..Default::default() },
        ..Default::default()
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
    let l1 = test_l1_block_ref(1000);
    let mut l2 = test_l2_block_ref(safe_number, safe_hash);
    l2.l1origin.hash = l1.hash;
    l2.l1origin.number = l1.number;
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
        signature: Bytes::from_static(&TEST_SIGNATURE),
        l1_origin_hash: B256::repeat_byte(0x02),
        l1_origin_number: 100 + block_number,
        l2_block_number: block_number,
        prev_output_root: B256::repeat_byte(0x03),
        config_hash: B256::repeat_byte(0x04),
        schedule_id: B256::repeat_byte(0x05),
    }
}

/// Mock prover-service requester for pipeline tests.
#[derive(Debug, Default)]
pub struct MockProofRequester {
    /// Requests accepted through `prove_block_range`.
    pub requests: Mutex<HashMap<String, ProveBlockRangeRequest>>,
    /// Sessions that should return a terminal failed status from `get_proof`.
    pub failed_sessions: Mutex<HashMap<String, String>>,
    /// Reject every `prove_block_range` call with an L1 head conflict.
    pub reject_l1_head_conflict: bool,
    /// Return a mismatched session id from `prove_block_range`.
    pub return_wrong_session_id: bool,
    /// Reject every `delete_proof_request` call with a timeout.
    pub reject_delete: bool,
}

#[async_trait]
impl ProofRequesterProvider for MockProofRequester {
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
        let session_id = request.proof.session_id.clone();
        if self.reject_l1_head_conflict {
            return Err(ProverServiceClientError::from(JsonRpcClientError::Call(
                ErrorObjectOwned::owned(
                    ProverServiceClientError::ERROR_FAILED_PRECONDITION,
                    ProofRequestIdCollisionMessage::for_field(session_id, "l1_head"),
                    None::<()>,
                ),
            )));
        }

        self.requests.lock().unwrap().insert(session_id.clone(), request);
        if self.return_wrong_session_id {
            return Ok(ProveBlockRangeResponse { session_id: "wrong-session".to_owned() });
        }

        Ok(ProveBlockRangeResponse { session_id })
    }

    async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> Result<GetProofResponse, ProverServiceClientError> {
        if let Some(message) = self.failed_sessions.lock().unwrap().get(&request.session_id) {
            return Ok(GetProofResponse {
                status: ProofStatus::Failed,
                error_message: Some(message.clone()),
                result: None,
            });
        }

        let requests = self.requests.lock().unwrap();
        let request = requests.get(&request.session_id).ok_or_else(|| {
            // Mirror the production prover-service: an unknown session_id surfaces
            // as a JSON-RPC NotFound error. The proposer pipeline relies
            // on this to distinguish "no session yet, dispatch needed" from other
            // transient or terminal errors.
            ProverServiceClientError::RpcTransport(JsonRpcClientError::Call(
                ErrorObjectOwned::owned(
                    ProverServiceClientError::ERROR_NOT_FOUND,
                    PROOF_REQUEST_NOT_FOUND_MESSAGE,
                    None::<()>,
                ),
            ))
        })?;
        let ApiProofRequestKind::Tee(tee_request) = &request.proof.request else {
            return Err(ProverServiceClientError::UnexpectedResultPayload(
                "expected TEE request".to_owned(),
            ));
        };
        let target = tee_request.proof.claimed_l2_block_number;
        let interval = tee_request.proof.intermediate_block_interval.max(1);
        let start = target.saturating_sub(interval);
        let proposals = ((start + 1)..=target).map(test_proposal).collect::<Vec<_>>();
        let aggregate_proposal = Proposal {
            output_root: tee_request.proof.claimed_l2_output_root,
            l1_origin_hash: tee_request.proof.l1_head,
            l1_origin_number: tee_request.proof.l1_head_number,
            l2_block_number: target,
            prev_output_root: tee_request.proof.agreed_l2_output_root,
            ..test_proposal(target)
        };
        Ok(GetProofResponse {
            status: ProofStatus::Succeeded,
            error_message: None,
            result: Some(ApiProofResult::Tee(TeeProofResult {
                aggregate_proposal,
                proposals,
                tee_kind: TeeKind::AwsNitro,
            })),
        })
    }

    async fn delete_proof_request(
        &self,
        request: DeleteProofRequest,
    ) -> Result<(), ProverServiceClientError> {
        if self.reject_delete {
            return Err(ProverServiceClientError::Timeout("simulated delete failure".into()));
        }

        self.requests.lock().unwrap().remove(&request.session_id);
        self.failed_sessions.lock().unwrap().remove(&request.session_id);
        Ok(())
    }

    async fn list_proofs(
        &self,
        _request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProverServiceClientError> {
        unimplemented!("tests do not list proofs")
    }
}

/// Mock output proposer that succeeds unless configured with a create error.
#[derive(Debug, Default)]
pub struct MockOutputProposer {
    /// Number of `propose_output()` calls.
    pub created: Mutex<u32>,
    /// Game addresses passed to `verify_proposal_proof()`.
    pub verified: Mutex<Vec<Address>>,
    /// Error returned by the next `propose_output` call.
    pub create_error: Mutex<Option<ProposerError>>,
    /// Error returned by the next `verify_proposal_proof` call.
    pub verify_error: Mutex<Option<ProposerError>>,
}

impl MockOutputProposer {
    /// Creates a mock that fails the next output proposal.
    pub fn with_create_error(error: ProposerError) -> Self {
        Self { create_error: Mutex::new(Some(error)), ..Default::default() }
    }
}

#[async_trait]
impl OutputProposer for MockOutputProposer {
    async fn propose_output(
        &self,
        _proposal: &Proposal,
        _parent_address: Address,
        _intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        *self.created.lock().unwrap() += 1;
        if let Some(error) = self.create_error.lock().unwrap().take() {
            return Err(error);
        }
        Ok(())
    }

    async fn verify_proposal_proof(
        &self,
        game_address: Address,
        _proposal: &Proposal,
    ) -> Result<(), ProposerError> {
        self.verified.lock().unwrap().push(game_address);
        if let Some(error) = self.verify_error.lock().unwrap().take() {
            return Err(error);
        }
        Ok(())
    }
}
