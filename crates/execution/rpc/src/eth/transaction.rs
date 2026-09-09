//! Loads and formats Base transaction RPC response.

use std::{future::Future, time::Duration};

use alloy_primitives::{B256, Bytes};
use base_common_chain_config::ChainSpecProvider;
use base_common_chain_config::Upgrades;
use base_common_types_chain::{BlockHeader, EIP8130_TX_TYPE_ID, Typed2718};
use base_common_types_rpc::BaseTransactionReceipt;
use base_execution_txpool::{AddedTransactionOutcome, TransactionOrigin, TransactionPool};
use base_common_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use futures::StreamExt;
use reth_chain_state::CanonStateSubscriptions;
use reth_primitives_traits::{SignerRecoverable, WithEncoded};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_types::{EthApiError, TransactionSource, block::convert_transaction_receipt};
use reth_storage_api::{BlockReaderIdExt, ProviderTx, TransactionsProvider};
use tracing::{debug, instrument, warn};

use crate::{
    BaseEthApi, BaseEthApiError, BaseInvalidTransactionError, SequencerClient, SignersForRpc,
};

impl BaseEthApi {
    pub fn signers(&self) -> &SignersForRpc<BlockchainProvider> {
        self.inner.signers()
    }

    pub fn send_raw_transaction_sync_timeout(&self) -> Duration {
        self.inner.send_raw_transaction_sync_timeout()
    }

    // Reth decodes and recovers raw RPC transactions into the pool's concrete transaction type
    // before invoking this hook. The original bytes remain available for broadcasting and
    // sequencer forwarding. `eth_sendRawTransaction` supplies a `Local` origin, so preserving
    // `origin` here intentionally applies the configured local-transaction pool policy.
    #[instrument(skip_all, fields(tx_hash = %tx.1.hash()))]
    pub async fn send_pool_transaction(
        &self,
        origin: TransactionOrigin,
        tx: WithEncoded<base_execution_txpool::BasePooledTransaction>,
    ) -> Result<B256, BaseEthApiError> {
        let (tx, pool_transaction) = tx.split();

        if pool_transaction.consensus_ref().ty() == EIP8130_TX_TYPE_ID
            && !self.is_zenith_active_at_latest()?
        {
            return Err(BaseInvalidTransactionError::Eip8130NotAccepted.into());
        }

        let tx_hash = *pool_transaction.hash();
        let _ = transaction_event!(
            producer: TransactionEventProducer::BaseRethNode,
            event_type: TransactionEventType::TxpoolSendRawTransaction,
            tx_hash: tx_hash,
            data: {
                "rpc_method" => "eth_sendRawTransaction",
            },
        );

        // broadcast raw transaction to subscribers if there is any.
        self.inner.broadcast_raw_transaction(tx.clone());

        // On Base, transactions are forwarded directly to the sequencer to be included in
        // blocks that it builds.
        if let Some(client) = self.raw_tx_forwarder().as_ref() {
            debug!(target: "rpc::eth", hash = %pool_transaction.hash(), "forwarding raw transaction to sequencer");
            let hash = client.forward_raw_transaction(&tx).await.inspect_err(|err| {
                    debug!(target: "rpc::eth", error = %err, hash=% *pool_transaction.hash(), "failed to forward raw transaction");
                })?;

            // Retain tx in local tx pool after forwarding, for local RPC usage.
            let _ = self.inner.add_pool_transaction(origin, pool_transaction).await.inspect_err(|err| {
                warn!(target: "rpc::eth", error = %err, %hash, "successfully sent tx to sequencer, but failed to persist in local tx pool");
            });

            return Ok(hash);
        }

        // submit the transaction to the pool with the given origin
        let AddedTransactionOutcome { hash, .. } = self
            .pool()
            .add_transaction(origin, pool_transaction)
            .await
            .map_err(BaseEthApiError::from_eth_err)?;

        Ok(hash)
    }

    /// Decodes and recovers the transaction and submits it to the pool.
    ///
    /// And awaits the receipt from canonical blocks.
    pub fn send_raw_transaction_sync(
        &self,
        tx: Bytes,
        timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<BaseTransactionReceipt, BaseEthApiError>> + Send {
        let this = self.clone();
        let configured_timeout = self.send_raw_transaction_sync_timeout();
        // A positive per-request timeout may shorten, but never extend, the configured maximum.
        // Zero or no timeout uses the configured value. This only bounds the wait for canonical
        // inclusion after submission; timing out does not cancel or remove the transaction.
        let timeout_duration = timeout_ms
            .filter(|timeout_ms| *timeout_ms > 0)
            .map(Duration::from_millis)
            .map(|timeout| timeout.min(configured_timeout))
            .unwrap_or(configured_timeout);
        async move {
            // Subscribe before submission so immediate inclusion cannot race the receipt listener.
            let mut canonical_stream = this.provider().canonical_state_stream();
            let hash = BaseEthApi::send_raw_transaction(&this, tx).await?;

            tokio::time::timeout(timeout_duration, async {
                while let Some(notification) = canonical_stream.next().await {
                    let chain = notification.committed();
                    if let Some((block, tx, receipt, all_receipts)) =
                        chain.find_transaction_and_receipt_by_hash(hash)
                        && let Some(receipt) = convert_transaction_receipt(
                            block,
                            all_receipts,
                            tx,
                            receipt,
                            this.converter(),
                        )
                        .transpose()?
                    {
                        return Ok(receipt);
                    }
                }
                Err(BaseEthApiError::from_eth_err(EthApiError::TransactionConfirmationTimeout {
                    hash,
                    duration: timeout_duration,
                }))
            })
            .await
            .unwrap_or_else(|_elapsed| {
                Err(BaseEthApiError::from_eth_err(EthApiError::TransactionConfirmationTimeout {
                    hash,
                    duration: timeout_duration,
                }))
            })
        }
    }

    /// Returns the transaction receipt for the given hash.
    pub fn transaction_receipt(
        &self,
        hash: B256,
    ) -> impl Future<Output = Result<Option<BaseTransactionReceipt>, BaseEthApiError>> + Send {
        let this = self.clone();
        async move {
            let Some((tx, meta, receipt, all_receipts, block)) =
                this.load_transaction_and_receipt(hash).await?
            else {
                return Ok(None);
            };
            this.build_transaction_receipt(tx, meta, receipt, all_receipts, block).await.map(Some)
        }
    }
}

impl BaseEthApi {
    pub async fn transaction_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<TransactionSource<ProviderTx<BlockchainProvider>>>, BaseEthApiError> {
        // 1. Try to find the transaction on disk (historical blocks)
        if let Some((tx, meta)) = self
            .spawn_blocking_io(move |this| {
                this.provider()
                    .transaction_by_hash_with_meta(hash)
                    .map_err(BaseEthApiError::from_eth_err)
            })
            .await?
        {
            let transaction = tx
                .try_into_recovered_unchecked()
                .map_err(|_| EthApiError::InvalidTransactionSignature)?;

            return Ok(Some(TransactionSource::Block {
                transaction,
                index: meta.index,
                block_hash: meta.block_hash,
                block_number: meta.block_number,
                block_timestamp: meta.timestamp,
                base_fee: meta.base_fee,
            }));
        }

        // 2. check local pool
        if let Some(tx) = self.pool().get(&hash).map(|tx| tx.transaction.clone_into_consensus()) {
            return Ok(Some(TransactionSource::Pool(tx)));
        }

        Ok(None)
    }
}

impl BaseEthApi {
    /// Returns the [`SequencerClient`] if one is set.
    pub fn raw_tx_forwarder(&self) -> Option<SequencerClient> {
        self.inner.sequencer_client.clone()
    }
}

impl BaseEthApi {
    fn is_zenith_active_at_latest(&self) -> Result<bool, BaseEthApiError> {
        let Some(header) = self.provider().latest_header()? else {
            return Ok(false);
        };
        Ok(self.provider().chain_spec().is_zenith_active_at_timestamp(header.timestamp()))
    }
}
