use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, Bytes, U64, U256};
use alloy_rpc_types_eth::{
    BlockOverrides, EIP1186AccountProofResponse, Filter, SyncStatus, state::StateOverride,
};
use alloy_serde::JsonStorageKey;
use base_common_rpc_types::{
    BaseBlockResponse, BaseLogResponse, BaseTransactionReceipt, BaseTransactionRequest,
};
use jsonrpsee::core::RpcResult as Result;
use reth_rpc_api::EngineEthApiServer;
/// Re-export for convenience
pub use reth_rpc_engine_api::EngineApi;
use serde_json::Value;
use tracing_futures::Instrument;

use crate::{BaseEthApi, EngineEthFilter, EthApiServer, QueryLimits, RpcNodeCore};

macro_rules! engine_span {
    () => {
        tracing::info_span!(target: "rpc", "engine")
    };
}

/// A wrapper type for the `EthApi` and `EthFilter` implementations that only expose the required
/// subset for the `eth_` namespace used in auth server alongside the `engine_` namespace.
#[derive(Debug, Clone)]
pub struct EngineEthApi<Eth, EthFilter> {
    eth: Eth,
    eth_filter: EthFilter,
}

impl<ApiNode: RpcNodeCore, EthFilter> EngineEthApi<BaseEthApi<ApiNode>, EthFilter> {
    /// Create a new `EngineEthApi` instance.
    pub const fn new(eth: BaseEthApi<ApiNode>, eth_filter: EthFilter) -> Self {
        Self { eth, eth_filter }
    }
}

#[async_trait::async_trait]
impl<ApiNode: RpcNodeCore, EthFilter>
    EngineEthApiServer<
        BaseTransactionRequest,
        BaseBlockResponse,
        BaseTransactionReceipt,
        BaseLogResponse,
    > for EngineEthApi<BaseEthApi<ApiNode>, EthFilter>
where
    EthFilter: EngineEthFilter<BaseLogResponse>,
{
    /// Handler for: `eth_syncing`
    fn syncing(&self) -> Result<SyncStatus> {
        let span = engine_span!();
        let _enter = span.enter();
        EthApiServer::syncing(&self.eth)
    }

    /// Handler for: `eth_chainId`
    async fn chain_id(&self) -> Result<Option<U64>> {
        let span = engine_span!();
        let _enter = span.enter();
        EthApiServer::chain_id(&self.eth).await
    }

    /// Handler for: `eth_blockNumber`
    fn block_number(&self) -> Result<U256> {
        let span = engine_span!();
        let _enter = span.enter();
        EthApiServer::block_number(&self.eth)
    }

    /// Handler for: `eth_call`
    async fn call(
        &self,
        request: BaseTransactionRequest,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<Bytes> {
        EthApiServer::call(&self.eth, request, block_id, state_overrides, block_overrides)
            .instrument(engine_span!())
            .await
    }

    /// Handler for: `eth_getCode`
    async fn get_code(&self, address: Address, block_id: Option<BlockId>) -> Result<Bytes> {
        EthApiServer::get_code(&self.eth, address, block_id).instrument(engine_span!()).await
    }

    /// Handler for: `eth_getBlockByHash`
    async fn block_by_hash(&self, hash: B256, full: bool) -> Result<Option<BaseBlockResponse>> {
        EthApiServer::block_by_hash(&self.eth, hash, full).instrument(engine_span!()).await
    }

    /// Handler for: `eth_getBlockByNumber`
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> Result<Option<BaseBlockResponse>> {
        EthApiServer::block_by_number(&self.eth, number, full).instrument(engine_span!()).await
    }

    async fn block_receipts(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Vec<BaseTransactionReceipt>>> {
        EthApiServer::block_receipts(&self.eth, block_id).instrument(engine_span!()).await
    }

    /// Handler for: `eth_sendRawTransaction`
    async fn send_raw_transaction(&self, bytes: Bytes) -> Result<B256> {
        EthApiServer::send_raw_transaction(&self.eth, bytes).instrument(engine_span!()).await
    }

    async fn transaction_receipt(&self, hash: B256) -> Result<Option<BaseTransactionReceipt>> {
        EthApiServer::transaction_receipt(&self.eth, hash).instrument(engine_span!()).await
    }

    /// Handler for `eth_getLogs`
    async fn logs(&self, filter: Filter) -> Result<Vec<BaseLogResponse>> {
        self.eth_filter.logs(filter, QueryLimits::no_limits()).instrument(engine_span!()).await
    }

    /// Handler for `eth_getProof`
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> Result<EIP1186AccountProofResponse> {
        EthApiServer::get_proof(&self.eth, address, keys, block_number)
            .instrument(engine_span!())
            .await
    }

    /// Handler for `eth_getMultiProof`
    async fn get_multi_proof(
        &self,
        targets: Vec<(Address, Vec<B256>)>,
        block_number: Option<BlockId>,
    ) -> Result<Vec<EIP1186AccountProofResponse>> {
        EthApiServer::get_multi_proof(&self.eth, targets, block_number)
            .instrument(engine_span!())
            .await
    }

    /// Handler for `eth_getBlockAccessListByBlockHash`
    async fn block_access_list_by_block_hash(&self, hash: B256) -> Result<Option<Value>> {
        EthApiServer::block_access_list_by_block_hash(&self.eth, hash)
            .instrument(engine_span!())
            .await
    }

    /// Handler for `eth_getBlockAccessListByBlockNumber`
    async fn block_access_list_by_block_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> Result<Option<Value>> {
        EthApiServer::block_access_list_by_block_number(&self.eth, block_number)
            .instrument(engine_span!())
            .await
    }

    /// Handler for `eth_getBlockAccessList`
    async fn block_access_list(&self, block_id: BlockId) -> Result<Option<Value>> {
        EthApiServer::block_access_list(&self.eth, block_id).instrument(engine_span!()).await
    }

    /// Handler for `getBlockAccessListRaw`
    async fn block_access_list_raw(&self, block: BlockId) -> Result<Option<Bytes>> {
        EthApiServer::block_access_list_raw(&self.eth, block).instrument(engine_span!()).await
    }
}
