use std::sync::Arc;
use std::time::Duration;

use crate::metrics::Metrics;
use crate::subscription::Flashblock;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, Sealable, TxHash, U256};
use alloy_rpc_types::simulate::{SimBlock, SimulatePayload, SimulatedBlock};
use alloy_rpc_types::state::{EvmOverrides, StateOverride, StateOverridesBuilder};
use alloy_rpc_types::{BlockOverrides, TransactionTrait};
use alloy_rpc_types::{BlockTransactions, Header};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::OpTransactionRequest;
use reth::providers::CanonStateSubscriptions;
use reth::rpc::server_types::eth::EthApiError::TransactionConfirmationTimeout;
use reth_rpc_eth_api::helpers::EthState;
use reth_rpc_eth_api::helpers::EthTransactions;
use reth_rpc_eth_api::helpers::{EthBlocks, EthCall};
use reth_rpc_eth_api::{helpers::FullEthApi, RpcBlock};
use reth_rpc_eth_api::{RpcReceipt, RpcTransaction};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{debug, trace, warn};

/// Core API for accessing flashblock state and data.
pub trait FlashblocksAPI {
    /// Retrieves the current block. If `full` is true, includes full transaction details.
    fn get_block(&self, full: bool) -> Option<RpcBlock<Optimism>>;

    /// Gets transaction receipt by hash.
    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>>;

    /// Gets transaction count (nonce) for an address.
    fn get_transaction_count(&self, address: Address) -> U256;

    /// Gets transaction details by hash.
    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<RpcTransaction<Optimism>>;

    /// Gets balance for an address. Returns None if address not updated in flashblocks.
    fn get_balance(&self, address: Address) -> Option<U256>;

    /// Creates a subscription to receive flashblock updates.
    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Flashblock>;

    fn get_state_overrides(&self) -> Option<StateOverride>;
}

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
    ) -> RpcResult<RpcReceipt<Optimism>>;

    #[method(name = "call")]
    async fn call(
        &self,
        transaction: OpTransactionRequest,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<alloy_primitives::Bytes>;

    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        transaction: OpTransactionRequest,
        block_number: Option<BlockId>,
        overrides: Option<StateOverride>,
    ) -> RpcResult<U256>;

    #[method(name = "simulateV1")]
    async fn simulate_v1(
        &self,
        opts: SimulatePayload<OpTransactionRequest>,
        block_number: Option<BlockId>,
    ) -> RpcResult<Vec<SimulatedBlock<RpcBlock<Optimism>>>>;
}

#[derive(Debug)]
pub struct EthApiExt<Eth, FB> {
    eth_api: Eth,
    flashblocks_state: Arc<FB>,
    metrics: Metrics,
}

impl<Eth, FB> EthApiExt<Eth, FB> {
    pub fn new(eth_api: Eth, flashblocks_state: Arc<FB>) -> Self {
        Self {
            eth_api,
            flashblocks_state,
            metrics: Metrics::default(),
        }
    }
}

#[async_trait]
impl<Eth, FB> EthApiOverrideServer for EthApiExt<Eth, FB>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static,
    FB: FlashblocksAPI + Send + Sync + 'static,
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
            Ok(self.flashblocks_state.get_block(full))
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

            let fb_count = self.flashblocks_state.get_transaction_count(address);
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
    ) -> RpcResult<RpcReceipt<Optimism>> {
        debug!(message = "rpc::send_raw_transaction_sync");

        let tx_hash = match EthTransactions::send_raw_transaction(&self.eth_api, transaction).await
        {
            Ok(hash) => hash,
            Err(e) => return Err(e.into()),
        };

        debug!(
            message = "rpc::send_raw_transaction_sync::sent_transaction",
            tx_hash = %tx_hash
        );

        const TIMEOUT_DURATION: Duration = Duration::from_secs(6);
        loop {
            tokio::select! {
                receipt = self.wait_for_flashblocks_receipt(tx_hash) => {
                    if let Some(receipt) = receipt {
                        return Ok(receipt);
                    } else {
                        continue
                    }
                }
                receipt = self.wait_for_canonical_receipt(tx_hash) => {
                        if let Some(receipt) = receipt {
                            return Ok(receipt);
                        } else {
                            continue
                        }
                    }
                _ = time::sleep(TIMEOUT_DURATION) => {
                    return Err(TransactionConfirmationTimeout {
                        hash: tx_hash,
                        duration: TIMEOUT_DURATION,
                    }.into_rpc_err());
                }
            }
        }
    }

    async fn call(
        &self,
        transaction: OpTransactionRequest,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<alloy_primitives::Bytes> {
        let block_id = block_number.unwrap_or_default();
        let mut pending_overrides = EvmOverrides::default();
        // If the call is to pending block use cached override (if they exist)
        if block_id.is_pending() {
            self.metrics.call.increment(1);
            pending_overrides.state = self.flashblocks_state.get_state_overrides();
        }

        // Apply user's overrides on top
        let mut state_overrides_builder =
            StateOverridesBuilder::new(pending_overrides.state.unwrap_or_default());
        state_overrides_builder =
            state_overrides_builder.extend(state_overrides.unwrap_or_default());
        let final_overrides = state_overrides_builder.build();

        // Delegate to the underlying eth_api
        EthCall::call(
            &self.eth_api,
            transaction,
            block_number,
            EvmOverrides::new(Some(final_overrides), block_overrides),
        )
        .await
        .map_err(Into::into)
    }

    async fn estimate_gas(
        &self,
        transaction: OpTransactionRequest,
        block_number: Option<BlockId>,
        overrides: Option<StateOverride>,
    ) -> RpcResult<U256> {
        let block_id = block_number.unwrap_or_default();
        // TODO: Uncomment this when Reth is bumped beyond 1.6.0 and our fix is merged
        // let mut pending_overrides = EvmOverrides::default();
        // // If the call is to pending block use cached override (if they exist)
        // if block_id.is_pending() {
        //     self.metrics.estimate_gas.increment(1);
        //     pending_overrides.state = self.flashblocks_state.get_state_overrides();
        // }

        // let mut state_overrides_builder =
        //     StateOverridesBuilder::new(pending_overrides.state.unwrap_or_default());
        // state_overrides_builder = state_overrides_builder.extend(overrides.unwrap_or_default());
        // let final_overrides = state_overrides_builder.build();

        // EthCall::estimate_gas_at(&self.eth_api, transaction, block_id, Some(final_overrides))
        //     .await
        //     .map_err(Into::into)

        // TODO: Remove this and uncomment above
        // For now, we just delegate to the underlying eth_api
        EthCall::estimate_gas_at(&self.eth_api, transaction, block_id, overrides)
            .await
            .map_err(Into::into)
    }

    async fn simulate_v1(
        &self,
        opts: SimulatePayload<OpTransactionRequest>,
        block_number: Option<BlockId>,
    ) -> RpcResult<Vec<SimulatedBlock<RpcBlock<Eth::NetworkTypes>>>> {
        let block_id = block_number.unwrap_or_default();
        let mut pending_overrides = EvmOverrides::default();

        // If the call is to pending block use cached override (if they exist)
        if block_id.is_pending() {
            self.metrics.simulate_v1.increment(1);
            pending_overrides.state = self.cache.get::<StateOverride>(&CacheKey::PendingOverrides);
        }

        // Prepend flashblocks pending overrides to the block state calls
        let mut block_state_calls: Vec<SimBlock<OpTransactionRequest>> = Vec::new();
        for sim_block in opts.block_state_calls {
            let mut state_overrides_builder =
                StateOverridesBuilder::new(pending_overrides.state.clone().unwrap_or_default());
            state_overrides_builder =
                state_overrides_builder.extend(sim_block.state_overrides.unwrap_or_default());
            let final_overrides = state_overrides_builder.build();

            let block_state_call = SimBlock {
                state_overrides: Some(final_overrides),
                ..sim_block
            };
            block_state_calls.push(block_state_call);
        }

        let payload = SimulatePayload {
            block_state_calls,
            ..opts
        };

        EthCall::simulate_v1(&self.eth_api, payload, Some(block_id))
            .await
            .map_err(Into::into)
    }
}

impl<Eth, FB> EthApiExt<Eth, FB>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    async fn wait_for_flashblocks_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        let mut receiver = self.flashblocks_state.subscribe_to_flashblocks();

        loop {
            match receiver.recv().await {
                Ok(flashblock) if flashblock.metadata.receipts.contains_key(&tx_hash) => {
                    debug!(message = "found receipt in flashblock", tx_hash = %tx_hash);
                    return self.flashblocks_state.get_transaction_receipt(tx_hash);
                }
                Ok(_) => {
                    trace!(message = "flashblock does not contain receipt", tx_hash = %tx_hash);
                }
                Err(RecvError::Closed) => {
                    debug!(message = "flashblocks receipt queue closed");
                    return None;
                }
                Err(RecvError::Lagged(_)) => {
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
