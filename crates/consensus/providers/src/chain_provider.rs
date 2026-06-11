//! Providers that use alloy provider types on the backend.

use std::{boxed::Box, num::NonZeroUsize, vec::Vec};

use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_eips::{BlockId, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Network, Provider, RootProvider, network::BlockResponse};
use alloy_rpc_client::{ClientRef, RpcClient};
use alloy_transport::{RpcError, TransportErrorKind};
use async_trait::async_trait;
use base_common_consensus::BaseReceiptEnvelope;
use base_common_network::Base;
use base_consensus_derive::{ChainProvider, PipelineError, PipelineErrorKind, ResetError};
use base_protocol::BlockInfo;
use lru::LruCache;
use reqwest::Client;

use crate::{L1_RPC_TIMEOUT, L1RpcProvider, L1TxFormat, Metrics};

/// The [`AlloyChainProvider`] is a concrete implementation of the [`ChainProvider`] trait, providing
/// data over Ethereum JSON-RPC using an alloy provider as the backend.
#[derive(Debug, Clone)]
pub struct AlloyChainProvider {
    /// The inner JSON-RPC provider.
    inner: TypedL1Provider,
    /// Whether to trust the RPC without verification.
    pub trust_rpc: bool,
    /// `header_by_hash` LRU cache.
    header_by_hash_cache: LruCache<B256, Header>,
    /// `receipts_by_hash_cache` LRU cache.
    receipts_by_hash_cache: LruCache<B256, Vec<Receipt>>,
    /// `block_info_and_transactions_by_hash` LRU cache.
    block_info_and_transactions_by_hash_cache: LruCache<B256, (BlockInfo, Vec<TxEnvelope>)>,
}

/// The L1 JSON-RPC provider.
///
/// A node derives from a single L1 chain, so the transaction format is fixed at construction. Neither
/// transaction envelope is a superset of the other — Ethereum has EIP-4844 blobs that Base lacks;
/// Base has deposit (`0x7E`) and EIP-8130 (`0x7D`) txs that Ethereum lacks — so the full-decode
/// path is necessarily format-specific.
#[derive(Debug, Clone)]
pub enum TypedL1Provider {
    /// Ethereum-typed provider; decodes standard blocks including EIP-4844.
    Ethereum(RootProvider),
    /// Base/OP-typed provider; decodes deposit and EIP-8130 entries the Ethereum envelopes reject.
    Base(RootProvider<Base>),
}

impl TypedL1Provider {
    /// Returns the RPC client used by the underlying provider.
    pub fn client(&self) -> ClientRef<'_> {
        match self {
            Self::Ethereum(p) => p.client(),
            Self::Base(p) => p.client(),
        }
    }

    /// Fetches raw 2718 transaction bytes for every transaction trie entry.
    pub async fn block_transaction_bytes_2718(
        &self,
        hash: B256,
    ) -> Result<Vec<Vec<u8>>, AlloyChainProviderError> {
        match self {
            Self::Ethereum(p) => fetch_block_transactions_2718(p, hash).await,
            Self::Base(p) => fetch_block_transactions_2718(p, hash).await,
        }
    }

    /// Returns the latest block number.
    async fn get_block_number(&self) -> Result<u64, RpcError<TransportErrorKind>> {
        match self {
            Self::Ethereum(p) => p.get_block_number().await,
            Self::Base(p) => p.get_block_number().await,
        }
    }

    /// Returns the chain ID.
    async fn get_chain_id(&self) -> Result<u64, RpcError<TransportErrorKind>> {
        match self {
            Self::Ethereum(p) => p.get_chain_id().await,
            Self::Base(p) => p.get_chain_id().await,
        }
    }

    /// Reads a storage slot at the given block. Format-agnostic.
    async fn get_storage_at(
        &self,
        address: Address,
        slot: U256,
        block: BlockId,
    ) -> Result<U256, RpcError<TransportErrorKind>> {
        match self {
            Self::Ethereum(p) => p.get_storage_at(address, slot).block_id(block).await,
            Self::Base(p) => p.get_storage_at(address, slot).block_id(block).await,
        }
    }

    /// Fetches the consensus [`Header`] for a block hash, header-only (no tx bodies).
    async fn header_by_hash(&self, hash: B256) -> Result<Header, AlloyChainProviderError> {
        base_metrics::time!(Metrics::request_duration("header_by_hash"), {
            match self {
                Self::Ethereum(p) => {
                    p.get_block_by_hash(hash).await.map(|b| b.map(|b| b.header.into_consensus()))
                }
                Self::Base(p) => p
                    .get_block_by_hash(hash)
                    .await
                    .map(|b| b.map(|b| b.header.into_inner().into_consensus())),
            }
        })
        .inspect_err(|_e| {
            Metrics::chain_rpc_errors("header_by_hash").increment(1);
        })?
        .ok_or(AlloyChainProviderError::BlockNotFound(hash.into()))
    }

    /// Fetches the consensus [`Header`] for a block number, header-only (no tx bodies).
    async fn header_by_number(&self, number: u64) -> Result<Header, AlloyChainProviderError> {
        base_metrics::time!(Metrics::request_duration("block_by_number"), {
            match self {
                Self::Ethereum(p) => p
                    .get_block_by_number(number.into())
                    .await
                    .map(|b| b.map(|b| b.header.into_consensus())),
                Self::Base(p) => p
                    .get_block_by_number(number.into())
                    .await
                    .map(|b| b.map(|b| b.header.into_inner().into_consensus())),
            }
        })
        .inspect_err(|_e| {
            Metrics::chain_rpc_errors("block_by_number").increment(1);
        })?
        .ok_or(AlloyChainProviderError::BlockNotFound(number.into()))
    }

    /// Fetches a block's receipts and reduces each to its consensus [`Receipt`].
    ///
    /// All receipts are kept (incl. the index-0 deposit on a Base L1) so block-wide log-index
    /// numbering is preserved.
    async fn receipts_by_hash(&self, hash: B256) -> Result<Vec<Receipt>, AlloyChainProviderError> {
        match self {
            Self::Base(p) => Ok(fetch_block_receipts(p, hash)
                .await?
                .into_iter()
                .map(|r| Receipt::from(BaseReceiptEnvelope::from(r)))
                .collect()),
            Self::Ethereum(p) => fetch_block_receipts(p, hash)
                .await?
                .into_iter()
                .map(|r| r.inner.into_primitives_receipt().as_receipt().cloned())
                .collect::<Option<Vec<_>>>()
                .ok_or(AlloyChainProviderError::ReceiptsConversion(hash)),
        }
    }

    /// Fetches a full block and returns its consensus [`Header`] and transactions.
    ///
    /// On a Base L1, deposit (`0x7E`) and EIP-8130 (`0x7D`) txs have no Ethereum representation,
    /// so [`TxEnvelope::try_from`] drops them. L3 batcher submissions cannot use EIP-8130 until
    /// the DA pipeline can inspect Base envelopes directly.
    async fn header_and_transactions_by_hash(
        &self,
        hash: B256,
    ) -> Result<(Header, Vec<TxEnvelope>), AlloyChainProviderError> {
        match self {
            Self::Base(p) => {
                let block = fetch_full_block(p, hash).await?;
                let header = block.header.into_inner().into_consensus();
                let transactions = block
                    .transactions
                    .into_transactions()
                    .filter_map(|t| TxEnvelope::try_from(t.inner.into_inner()).ok())
                    .collect();
                Ok((header, transactions))
            }
            Self::Ethereum(p) => {
                let block = fetch_full_block(p, hash)
                    .await?
                    .into_consensus()
                    .map_transactions(|t| t.inner.into_inner());
                Ok((block.header, block.body.transactions))
            }
        }
    }
}

/// Builds an L1 execution HTTP [`RootProvider`] for network `N` using the default
/// [`L1_RPC_TIMEOUT`] deadline. Mirrors [`L1RpcProvider::new_http`] for the non-Ethereum
/// (`Base`-typed) network, which the crate-level builder does not construct.
fn new_l1_http_provider<N: Network>(url: url::Url) -> RootProvider<N> {
    let client = Client::builder()
        .timeout(L1_RPC_TIMEOUT)
        .build()
        .expect("failed to build L1 RPC HTTP client");
    RootProvider::new(RpcClient::new_http_with_client(client, url))
}

/// Fetches a block's receipts over JSON-RPC with timing/error metrics. Generic over the network so
/// both providers share the fetch path; the caller converts the response into consensus receipts.
async fn fetch_block_receipts<N: Network>(
    provider: &RootProvider<N>,
    hash: B256,
) -> Result<Vec<N::ReceiptResponse>, AlloyChainProviderError> {
    base_metrics::time!(Metrics::request_duration("receipts_by_hash"), {
        provider.get_block_receipts(hash.into()).await
    })
    .inspect_err(|_e| {
        Metrics::chain_rpc_errors("receipts_by_hash").increment(1);
    })?
    .ok_or(AlloyChainProviderError::BlockNotFound(hash.into()))
}

/// Fetches a full block (with transactions) over JSON-RPC with timing/error metrics. Generic over
/// the network so both providers share the fetch path; the caller converts the response into a
/// [`BlockInfo`] and consensus transactions.
async fn fetch_full_block<N: Network>(
    provider: &RootProvider<N>,
    hash: B256,
) -> Result<N::BlockResponse, AlloyChainProviderError> {
    base_metrics::time!(Metrics::request_duration("block_by_hash"), {
        provider.get_block_by_hash(hash).full().await
    })
    .inspect_err(|_e| {
        Metrics::chain_rpc_errors("block_by_hash").increment(1);
    })?
    .ok_or(AlloyChainProviderError::BlockNotFound(hash.into()))
}

async fn fetch_block_transactions_2718<N: Network>(
    provider: &RootProvider<N>,
    hash: B256,
) -> Result<Vec<Vec<u8>>, AlloyChainProviderError>
where
    N::TxEnvelope: Encodable2718,
{
    let block = fetch_full_block(provider, hash).await?;
    let transactions = block
        .transactions()
        .as_transactions()
        .ok_or(AlloyChainProviderError::TransactionBodiesUnavailable(hash))?;

    Ok(transactions.iter().map(|tx| tx.as_ref().encoded_2718()).collect())
}

impl AlloyChainProvider {
    /// Creates a new [`AlloyChainProvider`] with the given alloy provider.
    ///
    /// ## Panics
    /// - Panics if `cache_size` is zero.
    pub fn new(inner: RootProvider, cache_size: usize) -> Self {
        Self::new_with_trust(inner, cache_size, true)
    }

    /// Creates a new [`AlloyChainProvider`] with the given alloy provider and trust setting.
    ///
    /// ## Panics
    /// - Panics if `cache_size` is zero.
    pub fn new_with_trust(inner: RootProvider, cache_size: usize, trust_rpc: bool) -> Self {
        Self {
            inner: TypedL1Provider::Ethereum(inner),
            trust_rpc,
            header_by_hash_cache: LruCache::new(NonZeroUsize::new(cache_size).unwrap()),
            receipts_by_hash_cache: LruCache::new(NonZeroUsize::new(cache_size).unwrap()),
            block_info_and_transactions_by_hash_cache: LruCache::new(
                NonZeroUsize::new(cache_size).unwrap(),
            ),
        }
    }

    /// Creates a new [`AlloyChainProvider`] from the provided [`url::Url`].
    ///
    /// The underlying HTTP provider uses the default [`crate::L1_RPC_TIMEOUT`] deadline.
    pub fn new_http(url: url::Url, cache_size: usize) -> Self {
        let inner = L1RpcProvider::new_http(url);
        Self::new(inner, cache_size)
    }

    /// Creates an [`AlloyChainProvider`] from a URL, decoding full tx/receipt reads in the given
    /// [`L1TxFormat`].
    pub fn new_http_with_format(
        url: url::Url,
        cache_size: usize,
        trust_rpc: bool,
        format: L1TxFormat,
    ) -> Self {
        let inner = match format {
            L1TxFormat::Base => TypedL1Provider::Base(new_l1_http_provider::<Base>(url)),
            L1TxFormat::Ethereum => TypedL1Provider::Ethereum(L1RpcProvider::new_http(url)),
        };
        Self {
            inner,
            trust_rpc,
            header_by_hash_cache: LruCache::new(NonZeroUsize::new(cache_size).unwrap()),
            receipts_by_hash_cache: LruCache::new(NonZeroUsize::new(cache_size).unwrap()),
            block_info_and_transactions_by_hash_cache: LruCache::new(
                NonZeroUsize::new(cache_size).unwrap(),
            ),
        }
    }

    /// Returns the latest L2 block number.
    pub async fn latest_block_number(&mut self) -> Result<u64, RpcError<TransportErrorKind>> {
        Metrics::chain_rpc_calls("block_number").increment(1);

        let result = base_metrics::time!(Metrics::request_duration("block_number"), {
            self.inner.get_block_number().await
        });

        if result.is_err() {
            Metrics::chain_rpc_errors("block_number").increment(1);
        }

        result
    }

    /// Returns the chain ID.
    pub async fn chain_id(&mut self) -> Result<u64, RpcError<TransportErrorKind>> {
        self.inner.get_chain_id().await
    }

    /// Reads a storage slot at the given block over the underlying provider.
    pub async fn get_storage_at(
        &self,
        address: Address,
        slot: U256,
        block: BlockId,
    ) -> Result<U256, RpcError<TransportErrorKind>> {
        self.inner.get_storage_at(address, slot, block).await
    }

    /// Verifies that a header's hash matches the expected hash when `trust_rpc` is false.
    fn verify_header_hash(
        &self,
        header: &Header,
        expected_hash: B256,
    ) -> Result<(), AlloyChainProviderError> {
        if self.trust_rpc {
            return Ok(());
        }

        let actual_hash = header.hash_slow();
        if actual_hash != expected_hash {
            return Err(AlloyChainProviderError::Transport(RpcError::Transport(
                TransportErrorKind::Custom(
                    format!(
                        "Header hash mismatch: expected {expected_hash:?}, got {actual_hash:?}"
                    )
                    .into(),
                ),
            )));
        }

        Ok(())
    }
}

/// An error for the [`AlloyChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum AlloyChainProviderError {
    /// Transport error
    #[error(transparent)]
    Transport(#[from] RpcError<TransportErrorKind>),
    /// Block not found.
    #[error("Block not found: {0}")]
    BlockNotFound(BlockId),
    /// Full block response contained hashes, not transaction bodies.
    #[error("Block transaction bodies unavailable: {0}")]
    TransactionBodiesUnavailable(B256),
    /// Failed to convert RPC receipts into consensus receipts.
    #[error("Failed to convert RPC receipts into consensus receipts: {0}")]
    ReceiptsConversion(B256),
}

impl From<AlloyChainProviderError> for PipelineErrorKind {
    fn from(e: AlloyChainProviderError) -> Self {
        match e {
            AlloyChainProviderError::Transport(e) => {
                Self::Temporary(PipelineError::Provider(format!("Transport error: {e}")))
            }
            AlloyChainProviderError::BlockNotFound(id) => {
                // A hash-based lookup returning not-found means the block was reorged out —
                // retrying will never succeed, so reset.
                // A number-based lookup returning not-found means the next L1 block hasn't
                // been produced yet — this is transient, so Temporary.
                match id {
                    BlockId::Hash(_) => ResetError::BlockNotFound(id).reset(),
                    BlockId::Number(_) => Self::Temporary(PipelineError::Provider(format!(
                        "L1 Block not found: {id}"
                    ))),
                }
            }
            AlloyChainProviderError::ReceiptsConversion(_) => {
                Self::Temporary(PipelineError::Provider(
                    "Failed to convert RPC receipts into consensus receipts".to_string(),
                ))
            }
            AlloyChainProviderError::TransactionBodiesUnavailable(hash) => Self::Temporary(
                PipelineError::Provider(format!("L1 block transaction bodies unavailable: {hash}")),
            ),
        }
    }
}

#[async_trait]
impl ChainProvider for AlloyChainProvider {
    type Error = AlloyChainProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        if let Some(header) = self.header_by_hash_cache.get(&hash) {
            Metrics::chain_cache_hits("header_by_hash").increment(1);
            return Ok(header.clone());
        }

        Metrics::chain_cache_misses("header_by_hash").increment(1);

        Metrics::chain_rpc_calls("header_by_hash").increment(1);

        let header = self.inner.header_by_hash(hash).await?;

        // Verify the header hash matches what we requested
        self.verify_header_hash(&header, hash)?;

        self.header_by_hash_cache.put(hash, header.clone());

        Metrics::cache_entries("header_by_hash").increment(1);

        Ok(header)
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        Metrics::chain_rpc_calls("block_by_number").increment(1);

        let header = self.inner.header_by_number(number).await?;

        let block_info = BlockInfo {
            hash: header.hash_slow(),
            number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
        };
        Ok(block_info)
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        if let Some(receipts) = self.receipts_by_hash_cache.get(&hash) {
            Metrics::chain_cache_hits("receipts_by_hash").increment(1);
            return Ok(receipts.clone());
        }

        Metrics::chain_cache_misses("receipts_by_hash").increment(1);

        Metrics::chain_rpc_calls("receipts_by_hash").increment(1);

        let consensus_receipts = self.inner.receipts_by_hash(hash).await?;

        self.receipts_by_hash_cache.put(hash, consensus_receipts.clone());

        Metrics::cache_entries("receipts_by_hash").increment(1);

        Ok(consensus_receipts)
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        if let Some(block_info_and_txs) = self.block_info_and_transactions_by_hash_cache.get(&hash)
        {
            Metrics::chain_cache_hits("block_info_and_tx").increment(1);
            return Ok(block_info_and_txs.clone());
        }

        Metrics::chain_cache_misses("block_info_and_tx").increment(1);

        Metrics::chain_rpc_calls("block_by_hash").increment(1);

        let (header, transactions) = self.inner.header_and_transactions_by_hash(hash).await?;

        // Verify the header hash matches what we requested
        self.verify_header_hash(&header, hash)?;

        let block_info = BlockInfo {
            hash, // Use the already verified hash instead of recomputing
            number: header.number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
        };

        self.block_info_and_transactions_by_hash_cache
            .put(hash, (block_info, transactions.clone()));

        Metrics::cache_entries("block_info_and_tx").increment(1);

        Ok((block_info, transactions))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use httpmock::{HttpMockRequest, HttpMockResponse, Method::POST, MockServer};
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn test_from_alloy_chain_provider_error() {
        // Transport errors are transient — retry makes sense.
        let kind: PipelineErrorKind =
            AlloyChainProviderError::Transport(alloy_transport::RpcError::Transport(
                alloy_transport::TransportErrorKind::Custom("timeout".into()),
            ))
            .into();
        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));

        // ReceiptsConversion is a transient decode failure.
        let kind: PipelineErrorKind =
            AlloyChainProviderError::ReceiptsConversion(Default::default()).into();
        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));

        // Hash-based BlockNotFound: the block was reorged out. Retrying will never succeed
        // — the pipeline must reset. Without this, the safe head stalls on L1 reorgs.
        let kind: PipelineErrorKind =
            AlloyChainProviderError::BlockNotFound(B256::default().into()).into();
        assert!(
            matches!(kind, PipelineErrorKind::Reset(_)),
            "hash-based BlockNotFound must map to Reset (block reorged out)"
        );

        // Number-based BlockNotFound: the next L1 block hasn't been mined yet. This is
        // transient — the pipeline must wait, not reset.
        let kind: PipelineErrorKind = AlloyChainProviderError::BlockNotFound(0u64.into()).into();
        assert!(
            matches!(kind, PipelineErrorKind::Temporary(_)),
            "number-based BlockNotFound must stay Temporary (block not yet produced)"
        );
    }

    fn json_rpc_response(req: &HttpMockRequest, result: Value) -> String {
        let id = serde_json::from_slice::<Value>(&req.body_vec())
            .ok()
            .and_then(|body| body.get("id").cloned())
            .unwrap_or(Value::Null);
        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
    }

    /// A type-`0x7e` deposit transaction, as a Base/OP JSON-RPC endpoint returns it.
    fn deposit_tx_json() -> Value {
        json!({
            "type": "0x7e",
            "hash": "0x096c03d72acb06339c9c7860d1c36b6451932ec0ff16fd34aa9e30a73a245e13",
            "nonce": "0x0",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "transactionIndex": "0x0",
            "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
            "to": "0x4200000000000000000000000000000000000015",
            "value": "0x0",
            "gasPrice": "0x0",
            "gas": "0xf4240",
            "input": "0x",
            "v": "0x0",
            "r": "0x0",
            "s": "0x0",
            "sourceHash": "0x990d7122a1f121f3a6bc45723e28f4921c269037a77e77ffee3c8585136d1a92",
            "mint": "0x0",
            "depositReceiptVersion": "0x1"
        })
    }

    fn base_block_with_txs(txs: Vec<Value>) -> Value {
        json!({
            "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "parentHash": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0x3333333333333333333333333333333333333333333333333333333333333333",
            "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": "0x2a",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0x1",
            "extraData": "0x",
            "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "nonce": "0x0000000000000000",
            "baseFeePerGas": "0x1",
            "transactions": txs,
            "uncles": [],
            "withdrawals": [],
            "blobGasUsed": "0x0",
            "excessBlobGas": "0x0"
        })
    }

    /// A Base-format block whose only transaction is a `0x7e` deposit.
    fn base_block_with_deposit() -> Value {
        base_block_with_txs(vec![deposit_tx_json()])
    }

    /// A type-`0x7e` deposit receipt, as a Base/OP JSON-RPC endpoint returns it.
    fn deposit_receipt_json() -> Value {
        json!({
            "type": "0x7e",
            "status": "0x1",
            "cumulativeGasUsed": "0x0",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "logs": [],
            "transactionHash": "0x096c03d72acb06339c9c7860d1c36b6451932ec0ff16fd34aa9e30a73a245e13",
            "transactionIndex": "0x0",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
            "to": "0x4200000000000000000000000000000000000015",
            "gasUsed": "0x0",
            "effectiveGasPrice": "0x0",
            "contractAddress": null,
            "depositNonce": "0x0",
            "depositReceiptVersion": "0x1"
        })
    }

    /// A type-`0x7d` EIP-8130 transaction, as a Base RPC endpoint returns it.
    ///
    /// Built to match the wire format produced by
    /// `base_common_rpc_types::Transaction::from_transaction` for the `Eip8130` variant.
    fn eip8130_tx_json() -> Value {
        json!({
            "type": "0x7d",
            "hash": "0x4242424242424242424242424242424242424242424242424242424242424242",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "transactionIndex": "0x1",
            "from": "0x0000000000000000000000000000000000000011",
            "gasPrice": "0x12a05f200",
            "tx": {
                "chainId": 8453,
                "sender": "0x0000000000000000000000000000000000000011",
                "nonceKey": "0x0",
                "nonceSequence": 7,
                "expiry": 0,
                "maxPriorityFeePerGas": "0x3b9aca00",
                "maxFeePerGas": "0x12a05f200",
                "gasLimit": 1_000_000,
                "accountChanges": [],
                "calls": [],
                "payer": null
            },
            "senderAuth": format!("0x{}", "ab".repeat(32)),
            "payerAuth": "0x"
        })
    }

    /// A type-`0x7d` EIP-8130 receipt, as a Base RPC endpoint returns it.
    fn eip8130_receipt_json() -> Value {
        json!({
            "type": "0x7d",
            "status": "0x1",
            "cumulativeGasUsed": "0x5208",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "logs": [],
            "transactionHash": "0x4242424242424242424242424242424242424242424242424242424242424242",
            "transactionIndex": "0x1",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "from": "0x0000000000000000000000000000000000000011",
            "to": null,
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x12a05f200",
            "contractAddress": null
        })
    }

    /// A minimal EIP-1559 (type `0x02`) transaction with well-formed signature fields, as a
    /// standard Ethereum-compatible RPC endpoint returns it.
    fn eip1559_tx_json() -> Value {
        json!({
            "type": "0x2",
            "hash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "transactionIndex": "0x1",
            "from": "0x0000000000000000000000000000000000000099",
            "to": "0x0000000000000000000000000000000000000088",
            "value": "0x0",
            "nonce": "0x5",
            "gas": "0x5208",
            "input": "0x",
            "chainId": "0x2105",
            "maxFeePerGas": "0x12a05f200",
            "maxPriorityFeePerGas": "0x3b9aca00",
            "accessList": [],
            "v": "0x1",
            "r": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
            "s": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
            "yParity": "0x1"
        })
    }

    /// A minimal EIP-1559 (type `0x02`) receipt.
    fn eip1559_receipt_json() -> Value {
        json!({
            "type": "0x2",
            "status": "0x1",
            "cumulativeGasUsed": "0xa410",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "logs": [{
                "address": "0x0000000000000000000000000000000000000088",
                "topics": ["0x0000000000000000000000000000000000000000000000000000000000000001"],
                "data": "0x",
                "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                "blockNumber": "0x2a",
                "transactionHash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "transactionIndex": "0x1",
                "logIndex": "0x0",
                "removed": false
            }],
            "transactionHash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "transactionIndex": "0x1",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "from": "0x0000000000000000000000000000000000000099",
            "to": "0x0000000000000000000000000000000000000088",
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x12a05f200",
            "contractAddress": null
        })
    }

    /// A Base-format block containing a deposit (`0x7E`) at index 0 and an EIP-8130 (`0x7D`) tx
    /// at index 1.
    fn base_block_with_eip8130() -> Value {
        base_block_with_txs(vec![deposit_tx_json(), eip8130_tx_json()])
    }

    /// A Base-format block containing a deposit at index 0, an EIP-1559 tx at index 1, and an
    /// EIP-8130 tx at index 2. This is the most realistic scenario: deposits + standard user
    /// txs + AA txs coexisting in a single block.
    fn base_block_mixed() -> Value {
        let mut eip1559 = eip1559_tx_json();
        eip1559["transactionIndex"] = json!("0x1");
        let mut eip8130 = eip8130_tx_json();
        eip8130["transactionIndex"] = json!("0x2");
        base_block_with_txs(vec![deposit_tx_json(), eip1559, eip8130])
    }

    fn base_provider(server: &MockServer) -> AlloyChainProvider {
        let url: url::Url = server.url("/").parse().unwrap();
        AlloyChainProvider::new_http_with_format(url, 16, true, L1TxFormat::Base)
    }

    /// Regression: a Base-format block carrying a `0x7e` deposit must decode through the
    /// `RootProvider<Base>` path, and the deposit must be dropped from the down-converted txs
    /// (never surfaced to the calldata scanner / batcher-auth path).
    #[tokio::test]
    async fn base_format_block_decodes_and_drops_deposit() {
        let server = MockServer::start_async().await;
        let block = base_block_with_deposit();
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByHash"}"#);
                then.respond_with(move |req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, block.clone()))
                        .build()
                });
            })
            .await;

        let mut provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let (info, txs) = provider.block_info_and_transactions_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(info.number, 0x2a);
        assert!(
            txs.is_empty(),
            "the 0x7e deposit must be decoded then dropped from the down-converted transactions"
        );
    }

    #[tokio::test]
    async fn base_format_block_transaction_bytes_keep_deposit() {
        let server = MockServer::start_async().await;
        let block = base_block_with_deposit();
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByHash"}"#);
                then.respond_with(move |req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, block.clone()))
                        .build()
                });
            })
            .await;

        let url: url::Url = server.url("/").parse().unwrap();
        let provider = TypedL1Provider::Base(RootProvider::<Base>::new_http(url));
        let txs = provider.block_transaction_bytes_2718(B256::repeat_byte(0x11)).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(txs.len(), 1, "host preimage tx fetch must retain trie entries");
        assert_eq!(txs[0].first().copied(), Some(0x7e));
    }

    /// Regression: a Base-format `0x7e` deposit receipt must decode and be kept — unlike the tx
    /// path, receipts preserve deposits, since the index-0 deposit anchors block-wide log indices.
    #[tokio::test]
    async fn base_format_receipts_decode_and_preserve_deposit() {
        let server = MockServer::start_async().await;
        let receipts = json!([deposit_receipt_json()]);
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.respond_with(move |req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, receipts.clone()))
                        .build()
                });
            })
            .await;

        let mut provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let receipts = provider.receipts_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(receipts.len(), 1, "the 0x7e deposit receipt must be decoded and preserved");
    }

    /// Regression: an EIP-8130 (`0x7D`) transaction in a Base-format block must be deserialized
    /// by the `RootProvider<Base>` path but dropped from the down-converted `Vec<TxEnvelope>`
    /// returned by `block_info_and_transactions_by_hash`.
    #[tokio::test]
    async fn base_format_block_decodes_and_drops_eip8130() {
        let server = MockServer::start_async().await;
        let block = base_block_with_eip8130();
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByHash"}"#);
                then.respond_with(move |req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, block.clone()))
                        .build()
                });
            })
            .await;

        let mut provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let (info, txs) = provider.block_info_and_transactions_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(info.number, 0x2a);
        assert!(
            txs.is_empty(),
            "both 0x7e deposit and 0x7d EIP-8130 must be dropped from down-converted transactions"
        );
    }

    /// Regression: an EIP-8130 (`0x7D`) receipt must be deserialized and preserved in the
    /// `Vec<Receipt>` returned by `receipts_by_hash`.
    #[tokio::test]
    async fn base_format_receipts_decode_and_preserve_eip8130() {
        let server = MockServer::start_async().await;
        let receipts = json!([deposit_receipt_json(), eip8130_receipt_json()]);
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.respond_with(move |req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, receipts.clone()))
                        .build()
                });
            })
            .await;

        let mut provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let receipts = provider.receipts_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(receipts.len(), 2, "both deposit and EIP-8130 receipts must be preserved");
        assert_eq!(
            receipts[1].status,
            alloy_consensus::Eip658Value::Eip658(true),
            "EIP-8130 receipt status must be correctly converted"
        );
    }

    /// When a Base-format block contains a mix of non-Ethereum txs (deposit, EIP-8130) and
    /// standard Ethereum-compatible txs (EIP-1559), only the standard txs survive the
    /// down-conversion while the non-Ethereum txs are dropped.
    #[tokio::test]
    async fn base_format_mixed_block_preserves_standard_txs() {
        let server = MockServer::start_async().await;
        let block = base_block_mixed();
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByHash"}"#);
                then.respond_with(move |req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, block.clone()))
                        .build()
                });
            })
            .await;

        let mut provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let (info, txs) = provider.block_info_and_transactions_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(info.number, 0x2a);
        assert_eq!(txs.len(), 1, "only the EIP-1559 tx should survive down-conversion");
        assert!(matches!(txs[0], TxEnvelope::Eip1559(_)), "surviving tx must be EIP-1559");
    }

    /// When a Base-format block contains receipts of mixed types, ALL receipts are preserved
    /// (including deposit and EIP-8130), maintaining block-wide log-index numbering.
    #[tokio::test]
    async fn base_format_mixed_receipts_preserve_all() {
        let server = MockServer::start_async().await;
        let receipts =
            json!([deposit_receipt_json(), eip1559_receipt_json(), eip8130_receipt_json()]);
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.respond_with(move |req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, receipts.clone()))
                        .build()
                });
            })
            .await;

        let mut provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let receipts = provider.receipts_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(receipts.len(), 3, "all three receipts must be preserved regardless of type");
        assert_eq!(
            receipts[0].status,
            alloy_consensus::Eip658Value::Eip658(true),
            "deposit receipt status"
        );
        assert_eq!(receipts[1].logs.len(), 1, "EIP-1559 receipt log preserved");
        assert_eq!(
            receipts[2].status,
            alloy_consensus::Eip658Value::Eip658(true),
            "EIP-8130 receipt status"
        );
    }
}
