//! L1 JSON-RPC provider.

use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_eips::{BlockId, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Network, Provider, RootProvider, network::BlockResponse};
use alloy_rpc_client::ClientRef;
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_consensus::BaseReceiptEnvelope;

use crate::Base;

/// The L1 JSON-RPC provider.
///
/// A node derives from a single L1 chain, so the transaction format is fixed at construction. Neither
/// transaction envelope is a superset of the other — Ethereum has EIP-4844 blobs that Base lacks;
/// Base has deposit (`0x7E`) and EIP-8130 (`0x7D`) txs that Ethereum lacks — so the full-decode
/// path is necessarily format-specific.
#[derive(Debug, Clone)]
pub enum L1RpcProvider {
    /// Ethereum-typed provider; decodes standard blocks including EIP-4844.
    Ethereum(RootProvider),
    /// Base/OP-typed provider; decodes deposit and EIP-8130 entries the Ethereum envelopes reject.
    Base(RootProvider<Base>),
}

impl L1RpcProvider {
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
    ) -> Result<Vec<Vec<u8>>, L1RpcProviderError> {
        match self {
            Self::Ethereum(p) => fetch_block_transactions_2718(p, hash).await,
            Self::Base(p) => fetch_block_transactions_2718(p, hash).await,
        }
    }

    /// Returns the latest block number.
    pub async fn get_block_number(&self) -> Result<u64, RpcError<TransportErrorKind>> {
        match self {
            Self::Ethereum(p) => p.get_block_number().await,
            Self::Base(p) => p.get_block_number().await,
        }
    }

    /// Returns the chain ID.
    pub async fn get_chain_id(&self) -> Result<u64, RpcError<TransportErrorKind>> {
        match self {
            Self::Ethereum(p) => p.get_chain_id().await,
            Self::Base(p) => p.get_chain_id().await,
        }
    }

    /// Reads a storage slot at the given block. Format-agnostic.
    pub async fn get_storage_at(
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
    pub async fn header_by_hash(&self, hash: B256) -> Result<Header, L1RpcProviderError> {
        match self {
            Self::Ethereum(p) => {
                p.get_block_by_hash(hash).await.map(|b| b.map(|b| b.header.into_consensus()))
            }
            Self::Base(p) => p
                .get_block_by_hash(hash)
                .await
                .map(|b| b.map(|b| b.header.into_inner().into_consensus())),
        }?
        .ok_or(L1RpcProviderError::BlockNotFound(hash.into()))
    }

    /// Fetches the consensus [`Header`] for a block number, header-only (no tx bodies).
    pub async fn header_by_number(&self, number: u64) -> Result<Header, L1RpcProviderError> {
        match self {
            Self::Ethereum(p) => p
                .get_block_by_number(number.into())
                .await
                .map(|b| b.map(|b| b.header.into_consensus())),
            Self::Base(p) => p
                .get_block_by_number(number.into())
                .await
                .map(|b| b.map(|b| b.header.into_inner().into_consensus())),
        }?
        .ok_or(L1RpcProviderError::BlockNotFound(number.into()))
    }

    /// Fetches a block's receipts and reduces each to its consensus [`Receipt`].
    ///
    /// All receipts are kept (incl. the index-0 deposit on a Base L1) so block-wide log-index
    /// numbering is preserved.
    pub async fn receipts_by_hash(&self, hash: B256) -> Result<Vec<Receipt>, L1RpcProviderError> {
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
                .ok_or(L1RpcProviderError::ReceiptsConversion(hash)),
        }
    }

    /// Fetches a full block and returns its consensus [`Header`] and transactions.
    ///
    /// On a Base L1, deposit (`0x7E`) and EIP-8130 (`0x7D`) txs have no Ethereum representation,
    /// so [`TxEnvelope::try_from`] drops them. L3 batcher submissions cannot use EIP-8130 until
    /// the DA pipeline can inspect Base envelopes directly.
    pub async fn header_and_transactions_by_hash(
        &self,
        hash: B256,
    ) -> Result<(Header, Vec<TxEnvelope>), L1RpcProviderError> {
        match self {
            Self::Base(p) => Ok(base_block_into_header_and_txs(fetch_full_block(p, hash).await?)),
            Self::Ethereum(p) => {
                let block = fetch_full_block(p, hash)
                    .await?
                    .into_consensus()
                    .map_transactions(|t| t.inner.into_inner());
                Ok((block.header, block.body.transactions))
            }
        }
    }

    /// Fetches a full block by number and returns its consensus [`Header`] and transactions.
    ///
    /// Applies the same Base down-conversion as [`Self::header_and_transactions_by_hash`].
    pub async fn header_and_transactions_by_number(
        &self,
        number: u64,
    ) -> Result<(Header, Vec<TxEnvelope>), L1RpcProviderError> {
        match self {
            Self::Base(p) => {
                Ok(base_block_into_header_and_txs(fetch_full_block_by_number(p, number).await?))
            }
            Self::Ethereum(p) => {
                let block = fetch_full_block_by_number(p, number)
                    .await?
                    .into_consensus()
                    .map_transactions(|t| t.inner.into_inner());
                Ok((block.header, block.body.transactions))
            }
        }
    }
}

/// Converts a Base full-block response into a consensus [`Header`] and Ethereum [`TxEnvelope`]s.
///
/// Deposit (`0x7E`) and EIP-8130 (`0x7D`) txs have no Ethereum `TxEnvelope` representation, so
/// [`TxEnvelope::try_from`] drops them. That conversion is an exhaustive match in
/// `base-common-consensus`, so a future Base tx variant forces a deliberate map/drop decision
/// there at compile time rather than vanishing silently here.
fn base_block_into_header_and_txs(
    block: <Base as Network>::BlockResponse,
) -> (Header, Vec<TxEnvelope>) {
    let header = block.header.into_inner().into_consensus();
    let transactions = block
        .transactions
        .into_transactions()
        .filter_map(|t| TxEnvelope::try_from(t.inner.into_inner()).ok())
        .collect();
    (header, transactions)
}

/// Fetches a block's receipts over JSON-RPC. Generic over the network so both providers share the
/// fetch path; the caller converts the response into consensus receipts.
async fn fetch_block_receipts<N: Network>(
    provider: &RootProvider<N>,
    hash: B256,
) -> Result<Vec<N::ReceiptResponse>, L1RpcProviderError> {
    provider
        .get_block_receipts(hash.into())
        .await?
        .ok_or(L1RpcProviderError::BlockNotFound(hash.into()))
}

/// Fetches a full block (with transactions) over JSON-RPC. Generic over the network so both
/// providers share the fetch path; the caller converts the response into consensus types.
async fn fetch_full_block<N: Network>(
    provider: &RootProvider<N>,
    hash: B256,
) -> Result<N::BlockResponse, L1RpcProviderError> {
    provider
        .get_block_by_hash(hash)
        .full()
        .await?
        .ok_or(L1RpcProviderError::BlockNotFound(hash.into()))
}

/// Fetches a full block by number (with transactions) over JSON-RPC. Generic over the network so
/// both providers share the fetch path; the caller converts the response into consensus types.
async fn fetch_full_block_by_number<N: Network>(
    provider: &RootProvider<N>,
    number: u64,
) -> Result<N::BlockResponse, L1RpcProviderError> {
    provider
        .get_block_by_number(number.into())
        .full()
        .await?
        .ok_or(L1RpcProviderError::BlockNotFound(number.into()))
}

async fn fetch_block_transactions_2718<N: Network>(
    provider: &RootProvider<N>,
    hash: B256,
) -> Result<Vec<Vec<u8>>, L1RpcProviderError>
where
    N::TxEnvelope: Encodable2718,
{
    let block = fetch_full_block(provider, hash).await?;
    let transactions = block
        .transactions()
        .as_transactions()
        .ok_or(L1RpcProviderError::TransactionBodiesUnavailable(hash))?;

    Ok(transactions.iter().map(|tx| tx.as_ref().encoded_2718()).collect())
}

/// An error for the [`L1RpcProvider`].
#[derive(Debug, thiserror::Error)]
pub enum L1RpcProviderError {
    /// Transport error.
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

#[cfg(test)]
mod tests {
    use alloy_provider::RootProvider;
    use httpmock::{HttpMockRequest, HttpMockResponse, Method::POST, MockServer};
    use serde_json::{Value, json};

    use super::*;

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

    /// A minimal EIP-1559 (type `0x02`) transaction with well-formed signature fields.
    fn eip1559_tx_json() -> Value {
        json!({
            "type": "0x2",
            "hash": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "transactionIndex": "0x2",
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

    fn base_provider(server: &MockServer) -> L1RpcProvider {
        let url: url::Url = server.url("/").parse().unwrap();
        L1RpcProvider::Base(RootProvider::<Base>::new_http(url))
    }

    fn mock_block_by_number(server: &MockServer, block: Value) {
        server.mock(|when, then| {
            when.method(POST).path("/").json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
            then.respond_with(move |req| {
                HttpMockResponse::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(json_rpc_response(req, block.clone()))
                    .build()
            });
        });
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

        let provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let (header, txs) = provider.header_and_transactions_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(header.number, 0x2a);
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
        let provider = L1RpcProvider::Base(RootProvider::<Base>::new_http(url));
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

        let provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let receipts = provider.receipts_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(receipts.len(), 1, "the 0x7e deposit receipt must be decoded and preserved");
    }

    /// Regression: an EIP-8130 (`0x7D`) transaction in a Base-format block must be deserialized
    /// by the `RootProvider<Base>` path but dropped from the down-converted `Vec<TxEnvelope>`
    /// returned by `header_and_transactions_by_hash`.
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

        let provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let (header, txs) = provider.header_and_transactions_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(header.number, 0x2a);
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

        let provider = base_provider(&server);
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

        let provider = base_provider(&server);
        let hash = B256::repeat_byte(0x11);
        let (header, txs) = provider.header_and_transactions_by_hash(hash).await.unwrap();

        mock.assert_calls_async(1).await;
        assert_eq!(header.number, 0x2a);
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

        let provider = base_provider(&server);
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

    /// A Base-format block fetched by number must decode through the `RootProvider<Base>` path, with
    /// the `0x7E` deposit and `0x7D` EIP-8130 txs dropped from the down-converted transactions while
    /// the standard EIP-1559 tx survives.
    #[tokio::test]
    async fn base_by_number_drops_deposit_and_eip8130_keeps_standard() {
        let server = MockServer::start_async().await;
        mock_block_by_number(&server, base_block_mixed());

        let url: url::Url = server.url("/").parse().unwrap();
        let provider = L1RpcProvider::Base(RootProvider::<Base>::new_http(url));
        let (header, txs) = provider.header_and_transactions_by_number(0x2a).await.unwrap();

        assert_eq!(header.number, 0x2a);
        assert_eq!(txs.len(), 1, "only the EIP-1559 tx survives down-conversion");
        assert!(matches!(txs[0], TxEnvelope::Eip1559(_)), "surviving tx must be EIP-1559");
    }
}
