//! Mock implementations for testing engine client functionality.

use std::{collections::HashMap, sync::Arc};

use alloy_eips::{BlockId, eip1898::BlockNumberOrTag};
use alloy_json_rpc::ErrorPayload;
use alloy_primitives::{Address, B256};
use alloy_provider::{EthGetBlock, ProviderCall};
use alloy_transport::{TransportError, TransportErrorKind};
use async_trait::async_trait;
use base_common_chain_config::RollupConfig;
use base_common_network::{Base, Ethereum, Network};
use base_common_types_payload::{
    BaseExecutionPayloadEnvelope, BasePayloadAttributes, ForkchoiceState, ForkchoiceUpdated,
    PayloadId, PayloadStatus,
};
use base_common_types_rpc::{
    BaseTransaction, Block, EIP1186AccountProofResponse, Transaction as EthTransaction,
};
use base_protocol::L2BlockInfo;
use tokio::sync::RwLock;

use crate::{EngineClient, EngineClientError};

type L2RpcBlock = <Base as Network>::BlockResponse;

fn l2_rpc_block(block: Block<BaseTransaction>) -> L2RpcBlock {
    block.map_header(Into::into)
}

/// Builder for creating test `MockEngineClient` instances with sensible defaults
pub fn test_engine_client_builder() -> MockEngineClientBuilder {
    MockEngineClientBuilder::new().with_config(Arc::new(RollupConfig::default()))
}

/// A configurable error for [`MockEngineClient::get_l2_block`].
#[derive(Debug, Clone)]
pub enum MockL2BlockError {
    /// JSON-RPC error response with a structured [`ErrorPayload`].
    ErrorResp(ErrorPayload),
    /// Transport-layer custom error whose `to_string()` contains the given string.
    Custom(String),
}

/// Mock storage for engine client responses.
///
/// Records native commands and provides scripted responses.
#[derive(Debug, Clone, Default)]
pub struct MockEngineStorage {
    /// Storage for block responses by tag.
    pub l2_blocks_by_label: HashMap<BlockNumberOrTag, L2RpcBlock>,
    /// Storage for block info responses by tag.
    pub block_info_by_tag: HashMap<BlockNumberOrTag, L2BlockInfo>,
    /// Whether the EL is actively syncing.
    pub el_syncing: bool,

    /// Response to block submission.
    pub payload_response: Option<PayloadStatus>,
    /// Most recent submitted payload.
    pub last_payload: Option<BaseExecutionPayloadEnvelope>,
    /// Response to forkchoice updates.
    pub forkchoice_response: Option<ForkchoiceUpdated>,
    /// Forkchoice requests and whether they start a build.
    pub forkchoice_requests: Vec<(ForkchoiceState, bool)>,
    /// Scripted forkchoice failure.
    pub forkchoice_error: Option<ErrorPayload>,
    /// Payload returned when resolving a build.
    pub built_payload: Option<BaseExecutionPayloadEnvelope>,

    // Storage for get_l1_block, get_l2_block, and get_proof
    /// Storage for L1 blocks by stringified `BlockId`.
    /// L1 blocks use standard Ethereum transactions.
    pub l1_blocks_by_id: HashMap<String, Block<EthTransaction>>,
    /// Number of executed L1 block requests by stringified `BlockId`.
    pub l1_block_calls_by_id: HashMap<String, u64>,
    /// Storage for L2 blocks by stringified `BlockId`.
    /// L2 blocks use Base transactions.
    pub l2_blocks_by_id: HashMap<String, L2RpcBlock>,
    /// Errors returned for L2 block requests by stringified `BlockId`.
    pub l2_block_errors_by_id: HashMap<String, MockL2BlockError>,
    /// Storage for proofs by (address, stringified `BlockId`) key.
    pub proofs_by_address: HashMap<(Address, String), EIP1186AccountProofResponse>,
}

/// Builder for constructing a [`MockEngineClient`] with pre-configured responses.
///
/// This builder allows you to set up mock responses before creating the client,
/// making it easier to write concise tests.
///
/// # Example
///
/// ```rust
/// use base_consensus_engine::test_utils::{MockEngineClient};
/// use base_common_chain_config::RollupConfig;
/// use base_common_types_payload::{ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus};
/// use alloy_primitives::B256;
/// use std::sync::Arc;
///
/// let mock = MockEngineClient::builder()
///     .with_config(Arc::new(RollupConfig::default()))
///     .with_payload_response(PayloadStatus {
///         status: PayloadStatusEnum::Valid,
///         latest_valid_hash: Some(B256::ZERO),
///     })
///     .build();
/// ```
#[derive(Debug)]
pub struct MockEngineClientBuilder {
    cfg: Option<Arc<RollupConfig>>,
    storage: MockEngineStorage,
}

impl MockEngineClientBuilder {
    /// Creates a new builder with default values.
    pub fn new() -> Self {
        Self { cfg: None, storage: MockEngineStorage::default() }
    }

    /// Sets the rollup configuration.
    pub fn with_config(mut self, cfg: Arc<RollupConfig>) -> Self {
        self.cfg = Some(cfg);
        self
    }

    /// Sets a block response for a specific tag.
    pub fn with_l2_block_by_label(
        mut self,
        tag: BlockNumberOrTag,
        block: Block<BaseTransaction>,
    ) -> Self {
        self.storage.l2_blocks_by_label.insert(tag, l2_rpc_block(block));
        self
    }

    /// Sets a block info response for a specific tag.
    pub fn with_block_info_by_tag(mut self, tag: BlockNumberOrTag, info: L2BlockInfo) -> Self {
        self.storage.block_info_by_tag.insert(tag, info);
        self
    }

    /// Sets the `eth_syncing` response.
    pub const fn with_el_syncing(mut self, syncing: bool) -> Self {
        self.storage.el_syncing = syncing;
        self
    }

    /// Sets the `submit_payload` response.
    pub fn with_payload_response(mut self, status: PayloadStatus) -> Self {
        self.storage.payload_response = Some(status);
        self
    }

    /// Sets the `update_forkchoice` response.
    pub fn with_forkchoice_response(mut self, response: ForkchoiceUpdated) -> Self {
        self.storage.forkchoice_response = Some(response);
        self
    }

    /// Sets an error to return for `update_forkchoice`.
    pub fn with_forkchoice_error(mut self, error: ErrorPayload) -> Self {
        self.storage.forkchoice_error = Some(error);
        self
    }

    /// Sets an L1 block response for a specific `BlockId`.
    pub fn with_l1_block(mut self, block_id: BlockId, block: Block<EthTransaction>) -> Self {
        let key = block_id_to_key(&block_id);
        self.storage.l1_blocks_by_id.insert(key, block);
        self
    }

    /// Sets an L2 block response for a specific `BlockId`.
    pub fn with_l2_block(mut self, block_id: BlockId, block: Block<BaseTransaction>) -> Self {
        let key = block_id_to_key(&block_id);
        self.storage.l2_blocks_by_id.insert(key, l2_rpc_block(block));
        self
    }

    /// Sets a proof response for a specific address and `BlockId`.
    pub fn with_proof(
        mut self,
        address: Address,
        block_id: BlockId,
        proof: EIP1186AccountProofResponse,
    ) -> Self {
        let key = block_id_to_key(&block_id);
        self.storage.proofs_by_address.insert((address, key), proof);
        self
    }

    /// Sets an error to return for `get_l2_block` for a specific `BlockId`.
    pub fn with_l2_block_error(mut self, block_id: BlockId, error: MockL2BlockError) -> Self {
        let key = block_id_to_key(&block_id);
        self.storage.l2_block_errors_by_id.insert(key, error);
        self
    }

    /// Builds the [`MockEngineClient`] with the configured values.
    ///
    /// # Panics
    ///
    /// Panics if any required fields (cfg) are not set.
    pub fn build(self) -> MockEngineClient {
        let cfg = self.cfg.expect("cfg must be set");

        MockEngineClient { cfg, storage: Arc::new(RwLock::new(self.storage)) }
    }
}

impl Default for MockEngineClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Mock implementation of the `EngineClient` trait for testing.
///
/// This mock allows tests to configure expected responses for all `EngineClient`
/// and `BaseEngineApi` methods. All responses are stored in a shared [`MockEngineStorage`]
/// protected by an `RwLock` for thread-safe access.
#[derive(Debug, Clone)]
pub struct MockEngineClient {
    /// The rollup configuration.
    cfg: Arc<RollupConfig>,
    /// Shared storage for mock responses.
    storage: Arc<RwLock<MockEngineStorage>>,
}

impl MockEngineClient {
    /// Converts an existing RPC fixture into a native sealed block for execution tests.
    pub fn native_block(block: L2RpcBlock) -> reth_primitives_traits::SealedBlock {
        let hash = block.header.hash;
        reth_primitives_traits::SealedBlock::new_unchecked(
            block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
            hash,
        )
    }

    /// Creates a new mock engine client with the given config.
    pub fn new(cfg: Arc<RollupConfig>) -> Self {
        Self { cfg, storage: Arc::new(RwLock::new(MockEngineStorage::default())) }
    }

    /// Creates a builder for constructing a mock engine client.
    pub fn builder() -> MockEngineClientBuilder {
        MockEngineClientBuilder::new()
    }

    /// Returns a reference to the mock storage for configuring responses.
    pub fn storage(&self) -> Arc<RwLock<MockEngineStorage>> {
        Arc::clone(&self.storage)
    }

    /// Sets a block response for a specific tag.
    pub async fn set_l2_block_by_label(
        &self,
        tag: BlockNumberOrTag,
        block: Block<BaseTransaction>,
    ) {
        self.storage.write().await.l2_blocks_by_label.insert(tag, l2_rpc_block(block));
    }

    /// Sets a block info response for a specific tag.
    pub async fn set_block_info_by_tag(&self, tag: BlockNumberOrTag, info: L2BlockInfo) {
        self.storage.write().await.block_info_by_tag.insert(tag, info);
    }

    /// Sets the `submit_payload` response.
    pub async fn set_payload_response(&self, status: PayloadStatus) {
        self.storage.write().await.payload_response = Some(status);
    }

    /// Returns the most recent `submit_payload` request, if any.
    pub async fn last_payload(&self) -> Option<BaseExecutionPayloadEnvelope> {
        self.storage.read().await.last_payload.clone()
    }

    /// Sets the `update_forkchoice` response.
    pub async fn set_forkchoice_response(&self, response: ForkchoiceUpdated) {
        self.storage.write().await.forkchoice_response = Some(response);
    }

    /// Sets an L1 block response for a specific `BlockId`.
    pub async fn set_l1_block(&self, block_id: BlockId, block: Block<EthTransaction>) {
        let key = block_id_to_key(&block_id);
        self.storage.write().await.l1_blocks_by_id.insert(key, block);
    }

    /// Sets an L2 block response for a specific `BlockId`.
    pub async fn set_l2_block(&self, block_id: BlockId, block: Block<BaseTransaction>) {
        let key = block_id_to_key(&block_id);
        self.storage.write().await.l2_blocks_by_id.insert(key, l2_rpc_block(block));
    }

    /// Sets a proof response for a specific address and `BlockId`.
    pub async fn set_proof(
        &self,
        address: Address,
        block_id: BlockId,
        proof: EIP1186AccountProofResponse,
    ) {
        let key = block_id_to_key(&block_id);
        self.storage.write().await.proofs_by_address.insert((address, key), proof);
    }

    /// Sets an error to return for `get_l2_block` for a specific `BlockId`.
    pub async fn set_l2_block_error(&self, block_id: BlockId, error: MockL2BlockError) {
        let key = block_id_to_key(&block_id);
        self.storage.write().await.l2_block_errors_by_id.insert(key, error);
    }
}

#[async_trait]
impl EngineClient for MockEngineClient {
    async fn submit_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Result<PayloadStatus, EngineClientError> {
        let mut storage = self.storage.write().await;
        storage.last_payload = Some(envelope);
        storage
            .payload_response
            .clone()
            .ok_or_else(|| {
                TransportErrorKind::custom_str("no submission response configured").into()
            })
            .map_err(EngineClientError::RpcError)
    }

    async fn update_forkchoice(
        &self,
        state: ForkchoiceState,
        attributes: Option<BasePayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineClientError> {
        let mut storage = self.storage.write().await;
        storage.forkchoice_requests.push((state, attributes.is_some()));
        if let Some(error) = storage.forkchoice_error.clone() {
            return Err(TransportError::ErrorResp(error).into());
        }
        storage
            .forkchoice_response
            .clone()
            .ok_or_else(|| {
                TransportErrorKind::custom_str("no forkchoice response configured").into()
            })
            .map_err(EngineClientError::RpcError)
    }

    async fn resolve_payload(
        &self,
        _id: PayloadId,
    ) -> Result<BaseExecutionPayloadEnvelope, EngineClientError> {
        self.storage
            .read()
            .await
            .built_payload
            .clone()
            .ok_or_else(|| TransportErrorKind::custom_str("no built payload configured").into())
            .map_err(EngineClientError::RpcError)
    }

    fn cfg(&self) -> &RollupConfig {
        self.cfg.as_ref()
    }

    fn get_l1_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        let storage = Arc::clone(&self.storage);
        let block_key = block_id_to_key(&block);

        EthGetBlock::new_provider(
            block,
            Box::new(move |_kind| {
                let storage = Arc::clone(&storage);
                let block_key = block_key.clone();

                ProviderCall::BoxedFuture(Box::pin(async move {
                    let mut storage_guard = storage.write().await;
                    *storage_guard.l1_block_calls_by_id.entry(block_key.clone()).or_default() += 1;
                    Ok(storage_guard.l1_blocks_by_id.get(&block_key).cloned())
                }))
            }),
        )
    }

    async fn get_l2_block(
        &self,
        block: BlockId,
    ) -> Result<Option<reth_primitives_traits::SealedBlock>, EngineClientError> {
        let block_key = block_id_to_key(&block);
        let storage = self.storage.read().await;
        if let Some(error) = storage.l2_block_errors_by_id.get(&block_key).cloned() {
            return Err(EngineClientError::RpcError(match error {
                MockL2BlockError::ErrorResp(payload) => TransportError::ErrorResp(payload),
                MockL2BlockError::Custom(message) => {
                    TransportErrorKind::custom_str(&message).into()
                }
            }));
        }
        Ok(storage.l2_blocks_by_id.get(&block_key).cloned().map(Self::native_block))
    }

    async fn storage_root(
        &self,
        address: Address,
        block: BlockId,
    ) -> Result<B256, EngineClientError> {
        self.storage
            .read()
            .await
            .proofs_by_address
            .get(&(address, block_id_to_key(&block)))
            .map(|proof| proof.storage_hash)
            .ok_or_else(|| {
                EngineClientError::RpcError(
                    TransportErrorKind::custom_str(
                        "No storage root configured for this account and block",
                    )
                    .into(),
                )
            })
    }

    async fn l2_block_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<reth_primitives_traits::SealedBlock>, EngineClientError> {
        let storage = self.storage.read().await;
        Ok(storage.l2_blocks_by_label.get(&numtag).cloned().map(Self::native_block))
    }

    async fn l2_block_info_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<L2BlockInfo>, EngineClientError> {
        let storage = self.storage.read().await;
        Ok(storage.block_info_by_tag.get(&numtag).copied())
    }

    async fn el_syncing(&self) -> Result<bool, EngineClientError> {
        Ok(self.storage.read().await.el_syncing)
    }
}

/// Helper function to convert `BlockId` to a string key for `HashMap` storage.
/// This is necessary because `BlockId` doesn't implement Hash.
fn block_id_to_key(block_id: &BlockId) -> String {
    match block_id {
        BlockId::Hash(hash) => format!("hash:{}", hash.block_hash),
        BlockId::Number(num) => format!("number:{num}"),
    }
}
