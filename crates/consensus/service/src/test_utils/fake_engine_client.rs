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
use alloy_primitives::{Address, B256};
use alloy_provider::{EthGetBlock, ProviderCall};
use alloy_transport::{TransportError, TransportErrorKind};
use async_trait::async_trait;
use base_common_chain_config::RollupConfig;
use base_common_network::{Ethereum, Network};
use base_common_types_payload::{
    BaseExecutionPayloadEnvelope, BasePayloadAttributes, ForkchoiceState, ForkchoiceUpdated,
    PayloadId, PayloadStatus,
};
use base_common_types_rpc::BaseBlockResponse;
use base_consensus_engine::{EngineClient, EngineClientError};
use base_protocol::L2BlockInfo;

/// Scripted response for an forkchoice call.
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
    /// `update_forkchoice` invocation.
    UpdateForkchoice {
        /// Forkchoice state sent to the EL.
        fcs: ForkchoiceState,
        /// Optional payload attributes sent with forkchoice.
        payload_attributes: Box<Option<BasePayloadAttributes>>,
    },
    /// `submit_payload` invocation.
    SubmitPayload(Box<BaseExecutionPayloadEnvelope>),
    /// `resolve_payload` invocation.
    ResolvePayload(PayloadId),
    /// `l2_block_info_by_label` invocation.
    L2BlockInfoByLabel(BlockNumberOrTag),
    /// `l2_block_by_label` invocation.
    L2BlockByLabel(BlockNumberOrTag),
}

#[derive(Debug, Default)]
pub struct FakeEngineClientState {
    calls: Vec<EngineClientCall>,
    l2_block_info_by_tag: HashMap<BlockNumberOrTag, L2BlockInfo>,
    l2_blocks_by_label: HashMap<BlockNumberOrTag, BaseBlockResponse>,
    scripted_forkchoice: VecDeque<ScriptedForkchoiceResponse>,
    scripted_payload: VecDeque<PayloadStatus>,
    single_payload: Option<PayloadStatus>,
    built_payload: Option<Result<BaseExecutionPayloadEnvelope, String>>,
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

    /// Appends scripted forkchoice responses to be consumed in call order.
    pub fn push_scripted_forkchoice(
        &self,
        scripted: impl IntoIterator<Item = ScriptedForkchoiceResponse>,
    ) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .scripted_forkchoice
            .extend(scripted);
    }

    /// Appends scripted `submit_payload` responses to be consumed in call order.
    pub fn push_scripted_payload(&self, scripted: impl IntoIterator<Item = PayloadStatus>) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .scripted_payload
            .extend(scripted);
    }

    /// Records a synthetic forkchoice call in the call log and consumes one scripted response.
    pub fn inject_forkchoice_call(&self, fork_choice_state: ForkchoiceState) {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::UpdateForkchoice {
            fcs: fork_choice_state,
            payload_attributes: Box::new(None),
        });
        let _ = state.scripted_forkchoice.pop_front();
    }

    /// Sets the `l2_block_info_by_label` response for a specific tag.
    pub fn set_l2_block_info_by_label(&self, tag: Eip1898BlockNumberOrTag, block: L2BlockInfo) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_block_info_by_tag
            .insert(tag, block);
    }

    /// Sets the `l2_block_by_label` response for a specific tag.
    pub fn set_l2_block_by_label(&self, tag: BlockNumberOrTag, block: BaseBlockResponse) {
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

    /// Scripts one fallback `submit_payload` response.
    pub fn with_payload_response(self, response: PayloadStatus) -> Self {
        self.state.lock().expect("FakeEngineClient state mutex poisoned").single_payload =
            Some(response);
        self
    }

    /// Scripts one fallback `resolve_payload` response.
    pub fn with_built_payload(
        self,
        response: Result<BaseExecutionPayloadEnvelope, String>,
    ) -> Self {
        self.state.lock().expect("FakeEngineClient state mutex poisoned").built_payload =
            Some(response);
        self
    }

    /// Sets the `l2_block_info_by_label` response for a specific tag.
    pub fn set_l2_block_info_by_label(&self, tag: BlockNumberOrTag, block: L2BlockInfo) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_block_info_by_tag
            .insert(tag, block);
    }

    /// Sets the `l2_block_by_label` response for a specific tag.
    pub fn set_l2_block_by_label(&self, tag: BlockNumberOrTag, block: BaseBlockResponse) {
        self.state
            .lock()
            .expect("FakeEngineClient state mutex poisoned")
            .l2_blocks_by_label
            .insert(tag, block);
    }
}

#[async_trait]
impl EngineClient for FakeEngineClient {
    async fn submit_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<PayloadStatus, EngineClientError> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::SubmitPayload(Box::new(payload)));
        Ok(state.scripted_payload.pop_front().or_else(|| state.single_payload.clone()).unwrap_or(
            PayloadStatus {
                status: base_common_types_payload::PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            },
        ))
    }

    async fn update_forkchoice(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<BasePayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineClientError> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::UpdateForkchoice {
            fcs: fork_choice_state,
            payload_attributes: Box::new(payload_attributes),
        });
        match state.scripted_forkchoice.pop_front().unwrap_or_else(|| {
            ScriptedForkchoiceResponse::Err(
                "FAKE_EXHAUSTED: no scripted forkchoice response available".to_string(),
            )
        }) {
            ScriptedForkchoiceResponse::Ok(value) => Ok(value),
            ScriptedForkchoiceResponse::Err(message) => {
                Err(EngineClientError::RpcError(TransportErrorKind::custom_str(&message).into()))
            }
        }
    }

    async fn resolve_payload(
        &self,
        payload_id: PayloadId,
    ) -> Result<BaseExecutionPayloadEnvelope, EngineClientError> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::ResolvePayload(payload_id));
        state
            .built_payload
            .clone()
            .unwrap_or_else(|| Err("no built payload scripted".to_string()))
            .map_err(|error| {
                EngineClientError::RpcError(TransportErrorKind::custom_str(&error).into())
            })
    }

    fn cfg(&self) -> &RollupConfig {
        self.cfg.as_ref()
    }

    async fn el_syncing(&self) -> Result<bool, EngineClientError> {
        Ok(false)
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

    async fn get_l2_block(
        &self,
        block: BlockId,
    ) -> Result<Option<base_consensus_engine::SealedBlock>, EngineClientError> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        let BlockId::Number(tag) = block else { return Ok(None) };
        state.calls.push(EngineClientCall::L2BlockByLabel(tag));
        Ok(state
            .l2_blocks_by_label
            .get(&tag)
            .cloned()
            .map(base_consensus_engine::test_utils::MockEngineClient::native_block))
    }

    async fn storage_root(
        &self,
        _address: Address,
        _block: BlockId,
    ) -> Result<B256, EngineClientError> {
        Err(EngineClientError::RpcError(
            TransportErrorKind::custom_str("storage roots are not scripted for FakeEngineClient")
                .into(),
        ))
    }

    async fn l2_block_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<base_consensus_engine::SealedBlock>, EngineClientError> {
        let mut state = self.state.lock().expect("FakeEngineClient state mutex poisoned");
        state.calls.push(EngineClientCall::L2BlockByLabel(numtag));
        Ok(state
            .l2_blocks_by_label
            .get(&numtag)
            .cloned()
            .map(base_consensus_engine::test_utils::MockEngineClient::native_block))
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
