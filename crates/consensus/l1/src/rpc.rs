use std::{net::SocketAddr, sync::Arc};

use alloy_consensus::{Header, Sealed};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U64};
use alloy_rpc_types_eth::{
    Block, BlockTransactions, Filter, Header as RpcHeader, Log, Transaction,
};
use jsonrpsee::{
    RpcModule,
    server::{Server, ServerHandle},
    types::{
        ErrorObjectOwned,
        error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE},
    },
};
use reth_chainspec::EthChainSpec;
use reth_ethereum_primitives::TransactionSigned;
use url::Url;

use crate::L1Node;

type RpcResult<T> = Result<T, ErrorObjectOwned>;

/// Serves the subset of the L1 execution JSON-RPC API that the L1 watcher and startup sync read,
/// backed by an [`L1Node`].
///
/// Derivation, sequencer L1 origin selection and blob fetching read [`crate::L1Provider`]
/// in process. State methods (`eth_call`, `eth_getStorageAt`, ...) are not served: the L1 node
/// holds no state.
#[derive(Debug, Clone)]
pub struct L1Rpc {
    /// Backing node.
    pub node: Arc<L1Node>,
}

impl L1Rpc {
    /// Starts the server on a random loopback port and returns its URL.
    ///
    /// The server stops when the returned handle is dropped.
    pub async fn start(self) -> eyre::Result<(Url, ServerHandle)> {
        let server = Server::builder().build(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let url = Url::parse(&format!("http://{}", server.local_addr()?))?;
        Ok((url, server.start(self.module()?)))
    }

    /// Builds the RPC module.
    pub fn module(self) -> eyre::Result<RpcModule<Self>> {
        let mut module = RpcModule::new(self);
        module.register_method("eth_chainId", |_, rpc, _| {
            RpcResult::Ok(U64::from(rpc.node.peers.chain_spec.chain().id()))
        })?;
        module.register_method("eth_blockNumber", |_, rpc, _| {
            RpcResult::Ok(U64::from(rpc.node.chain.read().tip().map_or(0, |tip| tip.number)))
        })?;
        module.register_async_method("eth_getBlockByNumber", |params, rpc, _| async move {
            let (number, _full): (BlockNumberOrTag, bool) = params.parse()?;
            RpcResult::Ok(rpc.header_by_number(number).await?.map(Self::header_block))
        })?;
        module.register_async_method("eth_getBlockByHash", |params, rpc, _| async move {
            let (hash, _full): (B256, bool) = params.parse()?;
            RpcResult::Ok(
                rpc.node
                    .canonical_header(hash)
                    .await
                    .map_err(Self::internal)?
                    .map(Self::header_block),
            )
        })?;
        module.register_async_method("eth_getLogs", |params, rpc, _| async move {
            let (filter,): (Filter,) = params.parse()?;
            // ponytail: only block-hash filters, the one shape the L1 watcher sends. Ranges need
            // a per-block loop with a span limit.
            let hash = filter.get_block_hash().ok_or_else(|| {
                ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    "embedded L1 only supports blockHash log filters",
                    None::<()>,
                )
            })?;
            let header = rpc
                .node
                .canonical_header(hash)
                .await
                .map_err(Self::internal)?
                .ok_or_else(|| Self::internal("unknown block"))?;
            if !filter.matches_bloom(header.logs_bloom) {
                return RpcResult::Ok(Vec::new());
            }
            let receipts = rpc.node.receipts(&header).await.map_err(Self::internal)?;
            let mut log_index = 0;
            let mut logs = Vec::new();
            for (tx_index, receipt) in receipts.iter().enumerate() {
                for log in &receipt.logs {
                    if filter.matches(log) {
                        logs.push(Log {
                            inner: log.clone(),
                            block_hash: Some(header.hash()),
                            block_number: Some(header.number),
                            block_timestamp: Some(header.timestamp),
                            transaction_hash: None,
                            transaction_index: Some(tx_index as u64),
                            log_index: Some(log_index),
                            removed: false,
                        });
                    }
                    log_index += 1;
                }
            }
            Ok(logs)
        })?;
        Ok(module)
    }

    async fn header_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<Sealed<Header>>> {
        match number {
            BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                Ok(self.node.chain.read().tip().cloned())
            }
            BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized => {
                let hash = *self.node.finalized.read();
                match hash {
                    Some(hash) => {
                        self.node.header_by_hash(hash).await.map(Some).map_err(Self::internal)
                    }
                    None => Ok(None),
                }
            }
            BlockNumberOrTag::Number(number) => {
                self.node.header_by_number(number).await.map_err(Self::internal)
            }
            BlockNumberOrTag::Earliest => Ok(None),
        }
    }

    /// ponytail: callers on this RPC path (`BlockStream`, `L1WatcherActor` and startup forkchoice
    /// sync) only read `block.header`; full transactions are served in process by
    /// [`crate::L1Provider`].
    fn header_block(header: Sealed<Header>) -> Block<Transaction<TransactionSigned>> {
        Block {
            header: RpcHeader::from_consensus(header, None, None),
            uncles: Vec::new(),
            transactions: BlockTransactions::Hashes(Vec::new()),
            withdrawals: None,
        }
    }

    fn internal(error: impl std::fmt::Display) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, error.to_string(), None::<()>)
    }
}
