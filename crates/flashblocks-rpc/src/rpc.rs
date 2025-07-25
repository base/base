use std::sync::Arc;
use std::time::Duration;

use crate::metrics::Metrics;
use crate::state::FlashblocksState;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::Address;
use alloy_primitives::TxHash;
use alloy_primitives::U256;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use op_alloy_network::Optimism;
use reth::providers::CanonStateSubscriptions;
use reth_rpc_eth_api::helpers::EthBlocks;
use reth_rpc_eth_api::helpers::EthState;
use reth_rpc_eth_api::helpers::EthTransactions;
use reth_rpc_eth_api::{helpers::FullEthApi, RpcBlock};
use reth_rpc_eth_api::{RpcReceipt, RpcTransaction};
use tokio::sync::broadcast::error;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tracing::debug;
use tracing::log::warn;

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    #[method(name = "getBlockByNumber")]
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<RpcBlock<Optimism>>>;

    #[method(name = "getTransactionReceipt")]
    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcReceipt<Optimism>>>;

    #[method(name = "getBalance")]
    async fn get_balance(&self, address: Address, block_number: Option<BlockId>)
        -> RpcResult<U256>;

    #[method(name = "getTransactionCount")]
    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256>;

    #[method(name = "getTransactionByHash")]
    async fn transaction_by_hash(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcTransaction<Optimism>>>;

    #[method(name = "sendRawTransactionSync")]
    async fn send_raw_transaction_sync(
        &self,
        transaction: alloy_primitives::Bytes,
    ) -> RpcResult<Option<RpcReceipt<Optimism>>>;
}

#[derive(Debug)]
pub struct EthApiExt<Eth> {
    eth_api: Eth,
    flashblocks_state: Arc<FlashblocksState>,
    metrics: Metrics,
    total_timeout_secs: u64,
}

impl<Eth> EthApiExt<Eth> {
    pub fn new(eth_api: Eth, cache: Arc<FlashblocksState>, total_timeout_secs: u64) -> Self {
        Self {
            eth_api,
            flashblocks_state: cache,
            metrics: Metrics::default(),
            total_timeout_secs,
        }
    }
}

#[async_trait]
impl<Eth> EthApiOverrideServer for EthApiExt<Eth>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static,
    jsonrpsee_types::error::ErrorObject<'static>: From<Eth::Error>,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<RpcBlock<Optimism>>> {
        debug!(
            message = "rpc::block_by_number",
            block_number = ?number
        );

        if number.is_pending() {
            self.metrics.get_block_by_number.increment(1);
            Ok(self.flashblocks_state.block_by_number(full))
        } else {
            EthBlocks::rpc_block(&self.eth_api, number.into(), full)
                .await
                .map_err(Into::into)
        }
    }

    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcReceipt<Optimism>>> {
        debug!(
            message = "rpc::block_by_number",
            tx_hash = %tx_hash
        );

        if let Some(fb_receipt) = self.flashblocks_state.get_transaction_receipt(tx_hash) {
            self.metrics.get_transaction_receipt.increment(1);
            return Ok(Some(fb_receipt));
        }

        EthTransactions::transaction_receipt(&self.eth_api, tx_hash)
            .await
            .map_err(Into::into)
    }

    async fn get_balance(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        debug!(
            message = "rpc::get_balance",
            address = %address
        );
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            self.metrics.get_balance.increment(1);
            if let Some(balance) = self.flashblocks_state.get_balance(address) {
                return Ok(balance);
            }
        }

        EthState::balance(&self.eth_api, address, block_number)
            .await
            .map_err(Into::into)
    }

    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        debug!(
            message = "rpc::get_transaction_count",
            address = %address,
        );

        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            self.metrics.get_transaction_count.increment(1);
            let latest_count = EthState::transaction_count(
                &self.eth_api,
                address,
                Some(BlockId::Number(BlockNumberOrTag::Latest)),
            )
            .await
            .map_err(Into::into)?;

            // get the current latest block number
            let latest_block_header =
                EthBlocks::rpc_block_header(&self.eth_api, BlockNumberOrTag::Latest.into())
                    .await
                    .map_err(Into::into)?;

            // TODO: We can probably clean this up.
            // Check if we have a block header
            let latest_block_number = if let Some(header) = latest_block_header {
                header.number
            } else {
                // If there's no latest block, return the current nonce without additions
                return Ok(latest_count);
            };

            let fb_count = self
                .flashblocks_state
                .get_transaction_count(latest_block_number, address);
            return Ok(latest_count + fb_count);
        }

        EthState::transaction_count(&self.eth_api, address, block_number)
            .await
            .map_err(Into::into)
    }

    async fn transaction_by_hash(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<RpcTransaction<Optimism>>> {
        debug!(
            message = "rpc::transaction_by_hash",
            tx_hash = %tx_hash
        );

        if let Some(fb_transaction) = self.flashblocks_state.get_transaction_by_hash(tx_hash) {
            self.metrics.get_transaction_receipt.increment(1);
            return Ok(Some(fb_transaction));
        }

        Ok(EthTransactions::transaction_by_hash(&self.eth_api, tx_hash)
            .await?
            .map(|tx| tx.into_transaction(self.eth_api.tx_resp_builder()))
            .transpose()?)
    }

    async fn send_raw_transaction_sync(
        &self,
        transaction: alloy_primitives::Bytes,
    ) -> RpcResult<Option<RpcReceipt<Optimism>>> {
        debug!(message = "rpc::send_raw_transaction_sync",);

        let tx_hash = match EthTransactions::send_raw_transaction(&self.eth_api, transaction).await
        {
            Ok(hash) => hash,
            Err(e) => return Err(e.into()),
        };

        debug!(
            message = "rpc::send_raw_transaction_sync::sent_transaction",
            tx_hash = %tx_hash
        );

        match self.wait_for_receipt(tx_hash).await {
            Some(receipt) => Ok(Some(receipt)),
            None => Ok(None),
        }
    }
}

impl<Eth> EthApiExt<Eth>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static,
{
    /// Wait for receipts from Flashblocks or canonical chain
    async fn wait_for_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        let total_timeout = Duration::from_secs(self.total_timeout_secs);

        tokio::select! {
            receipt = self.wait_for_flashblocks_receipt(tx_hash) => receipt,
            receipt = self.wait_for_canonical_receipt(tx_hash) => receipt,
            _ = tokio::time::sleep(total_timeout) => {
                debug!(
                    message = "receipt waiting routine timed out",
                    tx_hash = %tx_hash
                );
                None
            }
        }
    }

    async fn wait_for_flashblocks_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        let mut receiver = self.flashblocks_state.subscribe_to_receipts();

        loop {
            match receiver.recv().await {
                Ok(receipt_with_hash) if receipt_with_hash.tx_hash == tx_hash => {
                    return self.flashblocks_state.get_transaction_receipt(tx_hash);
                }
                Ok(_) => continue,
                Err(error::RecvError::Closed) => {
                    debug!(message = "flashblocks receipt queue closed");
                    return None;
                }
                Err(error::RecvError::Lagged(_)) => {
                    warn!("Flashblocks receipt queue lagged, maybe missing receipts");
                }
            }
        }
    }

    async fn wait_for_canonical_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        let mut stream =
            BroadcastStream::new(self.eth_api.provider().subscribe_to_canonical_state());

        while let Some(Ok(canon_state)) = stream.next().await {
            for (block_receipt, _) in canon_state.block_receipts() {
                for (canonical_tx_hash, _) in &block_receipt.tx_receipts {
                    if *canonical_tx_hash == tx_hash {
                        debug!(
                            message = "found receipt in canonical state",
                            tx_hash = %tx_hash
                        );
                        return EthTransactions::transaction_receipt(&self.eth_api, tx_hash)
                            .await
                            .ok()
                            .flatten();
                    }
                }
            }
        }
        None
    }
}
