//! In-memory fake [`base_consensus_engine::EngineClient`] with call-log-first behavior.
//!
//! This fake is intentionally distinct from `base_consensus_engine::test_utils::MockEngineClient`:
//! it prioritizes deterministic call capture so Tier-0 actor-integration tests can assert exactly
//! which Engine API requests were sent by the CL. Responses are still scriptable per call.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use alloy_eips::{BlockId, BlockNumberOrTag, eip1898::BlockNumberOrTag as Eip1898BlockNumberOrTag};
use alloy_network::{Ethereum, Network};
use alloy_primitives::{Address, B256, BlockHash, StorageKey};
use alloy_provider::{EthGetBlock, ProviderCall, RpcWithBlock};
use alloy_rpc_types_engine::{
    ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2,
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
};
use alloy_rpc_types_eth::{Block, EIP1186AccountProofResponse};
use alloy_transport::{TransportError, TransportErrorKind, TransportResult};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types::Transaction as BaseTransaction;
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelopeV3, BaseExecutionPayloadEnvelopeV4, BaseExecutionPayloadEnvelopeV5,
    BaseExecutionPayloadV4, BasePayloadAttributes,
};
use base_consensus_engine::{EngineClient, EngineClientError};
use base_protocol::L2BlockInfo;

/// Scripted response for an FCU-v3 call.
#[derive(Clone, Debug)]
pub enum ScriptedForkchoiceResponse {
    /// Return a successful FCU response.
    Ok(ForkchoiceUpdated),
    /// Return a transport error with the provided message.
    Err(String),
}

/// Recorded Engine client call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineClientCall {
    /// `fork_choice_updated_v3` invocation.
    ForkChoiceUpdatedV3 {
        /// Forkchoice state sent to the EL.
        fcs: ForkchoiceState,
        /// Optional payload attributes sent with FCU-v3.
        payload_attributes: Box<Option<BasePayloadAttributes>>,
    },
    /// `fork_choice_updated_v2` invocation.
    ForkChoiceUpdatedV2(ForkchoiceState),
    /// `new_payload_v3` invocation.
    NewPayloadV3(Box<ExecutionPayloadV3>),
    /// `get_payload_v3` invocation.
    GetPayloadV3(PayloadId),
    /// `l2_block_info_by_label` invocation.
    L2BlockInfoByLabel(BlockNumberOrTag),
    /// `l2_block_by_label` invocation.
    L2BlockByLabel(BlockNumberOrTag),
}

#[derive(Debug, Default)]
struct FakeEngineClientState {
    calls: Vec<EngineClientCall>,
    l2_block_info_by_tag: HashMap<BlockNumberOrTag, L2BlockInfo>,
    l2_blocks_by_label: HashMap<BlockNumberOrTag, Block<BaseTransaction>>,
    scripted_fcu_v3: VecDeque<ScriptedForkchoiceResponse>,
    scripted_new_payload_v3: VecDeque<PayloadStatus>,
    single_new_payload_v3: Option<PayloadStatus>,
    single_get_payload_v3: Option<Result<BaseExecutionPayloadEnvelopeV3, String>>,
}

/// Handle for inspecting and mutating a [`FakeEngineClient`].
#[derive(Clone, Debug)]
pub struct FakeEngineClientHandle {
    state: Arc<Mutex<FakeEngineClientState>>,
}

impl FakeEngineClientHandle {
    /// Returns the number of recorded calls without cloning the full log.
    pub fn call_count(&self) -> usize {
        self.state.lock().expect("FakeEngineClient state mutex poisoned").calls.len()
    }

    /// Returns all recorded calls in order.
    pub fn calls(&self) -> Vec<EngineClientCall> {
        self.state.lock().expect("FakeEngineClient state mutex poisoned").calls.clone()
    }

    /// Appends scripted FCU-v3 responses to be consumed in call order.
    pub fn push_scripted_fcu_v3(
        &self,
        scripted: impl IntoIterator<Item = ScriptedForkchoiceResponse>,
    ) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .scripted_fcu_v3
            .extend(scripted);
    }

    /// Appends scripted `new_payload_v3` responses to be consumed in call order.
    pub fn push_scripted_new_payload_v3(&self, scripted: impl IntoIterator<Item = PayloadStatus>) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .scripted_new_payload_v3
            .extend(scripted);
    }

    /// Records a synthetic FCU-v3 call in the call log and consumes one scripted response.
    pub fn inject_fcu_v3_call(&self, fork_choice_state: ForkchoiceState) {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::ForkChoiceUpdatedV3 {
            fcs: fork_choice_state,
            payload_attributes: Box::new(None),
        });
        let _ = state.scripted_fcu_v3.pop_front();
    }

    /// Sets the `l2_block_info_by_label` response for a specific tag.
    pub fn set_l2_block_info_by_label(
        &self,
        tag: Eip1898BlockNumberOrTag,
        block: L2BlockInfo,
    ) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_block_info_by_tag
            .insert(tag, block);
    }

    /// Sets the `l2_block_by_label` response for a specific tag.
    pub fn set_l2_block_by_label(&self, tag: BlockNumberOrTag, block: Block<BaseTransaction>) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_blocks_by_label
            .insert(tag, block);
    }
}

/// Deterministic in-memory `EngineClient` fake.
#[derive(Clone, Debug)]
pub struct FakeEngineClient {
    cfg: Arc<RollupConfig>,
    state: Arc<Mutex<FakeEngineClientState>>,
}

impl FakeEngineClient {
    /// Creates a new fake client with default response scripting.
    pub fn new(cfg: Arc<RollupConfig>) -> Self {
        Self { cfg, state: Arc::new(Mutex::new(FakeEngineClientState::default())) }
    }

    /// Returns a shared handle for scripted responses and call-log assertions.
    pub fn handle(&self) -> FakeEngineClientHandle {
        FakeEngineClientHandle { state: Arc::clone(&self.state) }
    }

    /// Scripts one fallback `new_payload_v3` response.
    pub fn with_new_payload_v3_response(self, response: PayloadStatus) -> Self {
        self.state.lock().expect("FakeEngineClient state mutex poisoned").single_new_payload_v3 =
            Some(response);
        self
    }

    /// Scripts one fallback `get_payload_v3` response.
    pub fn with_get_payload_v3_response(
        self,
        response: Result<BaseExecutionPayloadEnvelopeV3, String>,
    ) -> Self {
        self.state.lock().expect("FakeEngineClient state mutex poisoned").single_get_payload_v3 =
            Some(response);
        self
    }

    /// Sets the `l2_block_info_by_label` response for a specific tag.
    pub async fn set_l2_block_info_by_label(&self, tag: BlockNumberOrTag, block: L2BlockInfo) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_block_info_by_tag
            .insert(tag, block);
    }

    /// Blocking variant of [`Self::set_l2_block_info_by_label`].
    pub fn set_l2_block_info_by_label_blocking(&self, tag: BlockNumberOrTag, block: L2BlockInfo) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_block_info_by_tag
            .insert(tag, block);
    }

    /// Sets the `l2_block_by_label` response for a specific tag.
    pub async fn set_l2_block_by_label(
        &self,
        tag: BlockNumberOrTag,
        block: Block<BaseTransaction>,
    ) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_blocks_by_label
            .insert(tag, block);
    }
}

#[async_trait]
impl EngineClient for FakeEngineClient {
    fn cfg(&self) -> &RollupConfig {
        self.cfg.as_ref()
    }

    fn get_l1_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        EthGetBlock::new_provider(
            block,
            Box::new(|_| {
                ProviderCall::BoxedFuture(Box::pin(async {
                    Ok::<_, TransportError>(Some(<Ethereum as Network>::BlockResponse::default()))
                }))
            }),
        )
    }

    fn get_l2_block(&self, block: BlockId) -> EthGetBlock<<Base as Network>::BlockResponse> {
        let numtag = match block {
            BlockId::Number(numtag) => Some(numtag),
            _ => None,
        };
        let state = Arc::clone(&self.state);
        EthGetBlock::new_provider(
            block,
            Box::new(move |_| {
                let state = Arc::clone(&state);
                let numtag = numtag;
                ProviderCall::BoxedFuture(Box::pin(async move {
                    let mut state = state.lock().expect("FakeEngineClient state mutex poisoned");
                    let block = numtag.map_or_else(
                        || None,
                        |numtag| {
                            state.calls.push(EngineClientCall::L2BlockByLabel(numtag));
                            state.l2_blocks_by_label.get(&numtag).cloned()
                        },
                    );
                    Ok::<_, TransportError>(block)
                }))
            }),
        )
    }

    fn get_proof(
        &self,
        _address: Address,
        _keys: Vec<StorageKey>,
    ) -> RpcWithBlock<(Address, Vec<StorageKey>), EIP1186AccountProofResponse> {
        RpcWithBlock::new_provider(|_| {
            ProviderCall::BoxedFuture(Box::pin(async {
                Err(TransportError::from(TransportErrorKind::custom_str(
                    "proofs are not scripted for FakeEngineClient",
                )))
            }))
        })
    }

    async fn l2_block_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<Block<BaseTransaction>>, EngineClientError> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::L2BlockByLabel(numtag));
        Ok(state.l2_blocks_by_label.get(&numtag).cloned())
    }

    async fn l2_block_info_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<L2BlockInfo>, EngineClientError> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::L2BlockInfoByLabel(numtag));
        Ok(state.l2_block_info_by_tag.get(&numtag).copied())
    }
}

#[async_trait]
impl BaseEngineApi for FakeEngineClient {
    async fn new_payload_v2(
        &self,
        _payload: ExecutionPayloadInputV2,
    ) -> TransportResult<PayloadStatus> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "new_payload_v2 is not scripted in FakeEngineClient",
        )))
    }

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        _parent_beacon_block_root: B256,
    ) -> TransportResult<PayloadStatus> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::NewPayloadV3(Box::new(payload)));
        if let Some(response) = state.scripted_new_payload_v3.pop_front() {
            return Ok(response);
        }
        if let Some(response) = state.single_new_payload_v3.clone() {
            return Ok(response);
        }
        Ok(PayloadStatus {
            status: alloy_rpc_types_engine::PayloadStatusEnum::Valid,
            latest_valid_hash: None,
        })
    }

    async fn new_payload_v4(
        &self,
        _payload: BaseExecutionPayloadV4,
        _parent_beacon_block_root: B256,
    ) -> TransportResult<PayloadStatus> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "new_payload_v4 is not scripted in FakeEngineClient",
        )))
    }

    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        _payload_attributes: Option<BasePayloadAttributes>,
    ) -> TransportResult<ForkchoiceUpdated> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::ForkChoiceUpdatedV2(fork_choice_state));
        Err(TransportError::from(TransportErrorKind::custom_str(
            "fork_choice_updated_v2 is not scripted in FakeEngineClient",
        )))
    }

    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<BasePayloadAttributes>,
    ) -> TransportResult<ForkchoiceUpdated> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::ForkChoiceUpdatedV3 {
            fcs: fork_choice_state,
            payload_attributes: Box::new(payload_attributes),
        });
        let response = state.scripted_fcu_v3.pop_front().unwrap_or_else(|| {
            ScriptedForkchoiceResponse::Err(
                "FAKE_EXHAUSTED: no scripted FCU-v3 response available (test setup: preload more \
                 responses)"
                    .to_string(),
            )
        });
        match response {
            ScriptedForkchoiceResponse::Ok(value) => Ok(value),
            ScriptedForkchoiceResponse::Err(message) => {
                Err(TransportError::from(TransportErrorKind::custom_str(&message)))
            }
        }
    }

    async fn get_payload_v2(
        &self,
        _payload_id: PayloadId,
    ) -> TransportResult<ExecutionPayloadEnvelopeV2> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "get_payload_v2 is not scripted in FakeEngineClient",
        )))
    }

    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> TransportResult<BaseExecutionPayloadEnvelopeV3> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::GetPayloadV3(payload_id));
        let response = state.single_get_payload_v3.clone();
        match response {
            Some(Ok(payload)) => Ok(payload),
            Some(Err(error)) => Err(TransportError::from(TransportErrorKind::custom_str(&error))),
            None => Err(TransportError::from(TransportErrorKind::custom_str(
                "get_payload_v3 is not scripted in FakeEngineClient",
            ))),
        }
    }

    async fn get_payload_v4(
        &self,
        _payload_id: PayloadId,
    ) -> TransportResult<BaseExecutionPayloadEnvelopeV4> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "get_payload_v4 is not scripted in FakeEngineClient",
        )))
    }

    async fn get_payload_v5(
        &self,
        _payload_id: PayloadId,
    ) -> TransportResult<BaseExecutionPayloadEnvelopeV5> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "get_payload_v5 is not scripted in FakeEngineClient",
        )))
    }

    async fn get_payload_bodies_by_hash_v1(
        &self,
        _block_hashes: Vec<BlockHash>,
    ) -> TransportResult<ExecutionPayloadBodiesV1> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "get_payload_bodies_by_hash_v1 is not scripted in FakeEngineClient",
        )))
    }

    async fn get_payload_bodies_by_range_v1(
        &self,
        _start: u64,
        _count: u64,
    ) -> TransportResult<ExecutionPayloadBodiesV1> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "get_payload_bodies_by_range_v1 is not scripted in FakeEngineClient",
        )))
    }

    async fn get_client_version_v1(
        &self,
        _client_version: ClientVersionV1,
    ) -> TransportResult<Vec<ClientVersionV1>> {
        Ok(Vec::new())
    }

    async fn exchange_capabilities(
        &self,
        capabilities: Vec<String>,
    ) -> TransportResult<Vec<String>> {
        Ok(capabilities)
    }
}
