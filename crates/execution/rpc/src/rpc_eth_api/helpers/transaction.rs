//! Database access for `eth_` transaction RPC methods. Loads transaction and receipt data w.r.t.
//! network.

use std::sync::Arc;

use alloy_dyn_abi::TypedData;
use alloy_eips::{BlockId, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bytes, TxHash, U256};
use base_common_types_chain::{
    BaseTxEnvelope, BlockHeader, Transaction,
    transaction::{SignerRecoverable, TransactionMeta},
};
use base_common_network::{TransactionBuilder, TransactionBuilder4844};
use base_common_rpc_types::{BaseTransactionRequest, TransactionInfo, state::EvmOverrides};
use base_execution_txpool::{
    AddedTransactionOutcome, PoolPooledTx, PoolTx, TransactionOrigin, TransactionPool,
};
use futures::Future;
use reth_primitives_traits::{Recovered, RecoveredBlock, SignedTransaction, WithEncoded};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_convert::TransactionConversionError;
use reth_rpc_eth_types::{
    BaseEthApiError,
    EthApiError::{self},
    FillTransaction, SignError, TransactionSource,
    utils::binary_search,
};
use reth_storage_api::{
    BlockNumReader, BlockReaderIdExt, ProviderReceipt, ProviderTx, ReceiptProvider,
    TransactionsProvider,
};

use super::EthSigner;
use crate::BaseEthApi;

/// Transaction related functions for the [`EthApiServer`](crate::EthApiServer) trait in
/// the `eth_` namespace.
///
/// This includes utilities for transaction tracing, transacting and inspection.
///
/// Async functions that are spawned onto the
/// [`BlockingTaskPool`](reth_tasks::pool::BlockingTaskPool) begin with `spawn_`
///
/// ## Calls
///
/// There are subtle differences between when transacting [`RpcTxReq`]:
///
/// The endpoints `eth_call` and `eth_estimateGas` and `eth_createAccessList` should always
/// __disable__ the base fee check in the EVM environment.
///
/// The behaviour for tracing endpoints is not consistent across clients.
/// Geth also disables the basefee check for tracing: <https://github.com/ethereum/go-ethereum/blob/bc0b87ca196f92e5af49bd33cc190ef0ec32b197/eth/tracers/api.go#L955-L955>
/// Erigon does not: <https://github.com/ledgerwatch/erigon/blob/aefb97b07d1c4fd32a66097a24eddd8f6ccacae0/turbo/transactions/tracing.go#L209-L209>
///
/// See also <https://github.com/paradigmxyz/reth/issues/6240>
///
/// This implementation follows the behaviour of Geth and disables the basefee check for tracing.
impl BaseEthApi {
    /// Returns a list of addresses owned by provider.
    pub fn accounts(&self) -> Vec<Address> {
        self.signers().read().iter().flat_map(|s| s.accounts()).collect()
    }

    /// Decodes and recovers the transaction and submits it to the pool.
    ///
    /// Returns the hash of the transaction.
    pub fn send_raw_transaction(
        &self,
        tx: Bytes,
    ) -> impl Future<Output = Result<B256, BaseEthApiError>> + Send {
        async move {
            let pool_transaction =
                PoolTx::recover_raw_transaction(&tx).map_err(BaseEthApiError::from_eth_err)?;
            self.send_pool_transaction(
                TransactionOrigin::Local,
                WithEncoded::new(tx, pool_transaction),
            )
            .await
        }
    }

    /// Submits the transaction to the pool with the given [`TransactionOrigin`].
    pub fn send_transaction(
        &self,
        origin: TransactionOrigin,
        tx: WithEncoded<Recovered<PoolPooledTx>>,
    ) -> impl Future<Output = Result<B256, BaseEthApiError>> + Send {
        async move {
            let (encoded, recovered) = tx.split();
            let pool_transaction =
                base_execution_txpool::BasePooledTransaction::from_pooled(recovered);

            self.send_pool_transaction(origin, WithEncoded::new(encoded, pool_transaction)).await
        }
    }

    /// Returns all transactions from the local pending pool.
    pub fn pending_transactions(
        &self,
    ) -> Result<Vec<base_common_rpc_types::BaseTransaction>, BaseEthApiError> {
        self.pool()
            .pending_transactions()
            .into_iter()
            .map(|tx| self.converter().fill_pending(tx.transaction.clone_into_consensus()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(BaseEthApiError::from)
    }

    /// Get all transactions in the block with the given hash.
    ///
    /// Returns `None` if block does not exist.
    #[expect(clippy::type_complexity)]
    pub fn transactions_by_block(
        &self,
        block: B256,
    ) -> impl Future<Output = Result<Option<Vec<ProviderTx<BlockchainProvider>>>, BaseEthApiError>> + Send
    {
        async move {
            self.cache()
                .get_recovered_block(block)
                .await
                .map(|b| b.map(|b| b.body().transactions.to_vec()))
                .map_err(BaseEthApiError::from_eth_err)
        }
    }

    /// Returns the EIP-2718 encoded transaction by hash.
    ///
    /// If this is a pooled EIP-4844 transaction, the blob sidecar is included.
    ///
    /// Checks the pool and state.
    ///
    /// Returns `Ok(None)` if no matching transaction was found.
    pub fn raw_transaction_by_hash(
        &self,
        hash: B256,
    ) -> impl Future<Output = Result<Option<Bytes>, BaseEthApiError>> + Send {
        async move {
            // Note: this is mostly used to fetch pooled transactions so we check the pool first
            if let Some(tx) =
                self.pool().get_pooled_transaction_element(hash).map(|tx| tx.encoded_2718().into())
            {
                return Ok(Some(tx));
            }

            self.spawn_blocking_io(move |ref this| {
                Ok(this
                    .provider()
                    .transaction_by_hash(hash)
                    .map_err(BaseEthApiError::from_eth_err)?
                    .map(|tx| tx.encoded_2718().into()))
            })
            .await
        }
    }

    /// Returns the _historical_ transaction and the block it was mined in
    #[expect(clippy::type_complexity)]
    pub fn historical_transaction_by_hash_at(
        &self,
        hash: B256,
    ) -> impl Future<
        Output = Result<
            Option<(TransactionSource<ProviderTx<BlockchainProvider>>, B256)>,
            BaseEthApiError,
        >,
    > + Send {
        async move {
            match self.transaction_by_hash_at(hash).await? {
                None => Ok(None),
                Some((tx, at)) => Ok(at.as_block_hash().map(|hash| (tx, hash))),
            }
        }
    }

    /// Helper method that loads a transaction and its receipt.
    ///
    /// The returned transaction has its sender already recovered.
    #[expect(clippy::complexity)]
    pub fn load_transaction_and_receipt(
        &self,
        hash: TxHash,
    ) -> impl Future<
        Output = Result<
            Option<(
                Recovered<ProviderTx<BlockchainProvider>>,
                TransactionMeta,
                ProviderReceipt<BlockchainProvider>,
                Option<Arc<Vec<ProviderReceipt<BlockchainProvider>>>>,
                Option<Arc<RecoveredBlock>>,
            )>,
            BaseEthApiError,
        >,
    > + Send
    where
        Self: 'static,
    {
        async move {
            if let Some(cached) = self.cache().get_transaction_by_hash(hash).await
                && let Some(tx) = cached.recovered_transaction().map(|tx| tx.cloned())
            {
                let meta = cached.transaction_meta(hash);

                // Best case: receipts are also cached.
                if let Some(all_receipts) = cached.receipts.clone()
                    && let Some(receipt) = all_receipts.get(cached.tx_index).cloned()
                {
                    return Ok(Some((tx, meta, receipt, Some(all_receipts), Some(cached.block))));
                }

                // Block still cached but receipts evicted — fetch via cache since
                // `build_transaction_receipt` needs all receipts for gas accounting
                // anyway.
                if let Some(receipts) = self
                    .cache()
                    .get_receipts(cached.block.hash())
                    .await
                    .map_err(BaseEthApiError::from_eth_err)?
                    && let Some(receipt) = receipts.get(cached.tx_index).cloned()
                {
                    return Ok(Some((tx, meta, receipt, Some(receipts), Some(cached.block))));
                }
            }

            // Full cache miss — fetch both from provider.
            self.spawn_blocking_io(move |this| {
                let provider = this.provider();
                let Some((tx, meta)) = provider
                    .transaction_by_hash_with_meta(hash)
                    .map_err(BaseEthApiError::from_eth_err)?
                else {
                    return Ok(None);
                };

                let tx =
                    tx.try_into_recovered_unchecked().map_err(BaseEthApiError::from_eth_err)?;

                let receipt =
                    provider.receipt_by_hash(hash).map_err(BaseEthApiError::from_eth_err)?;

                Ok(receipt.map(|receipt| (tx, meta, receipt, None, None)))
            })
            .await
        }
    }

    /// Get transaction by [`BlockId`] and index of transaction within that block.
    ///
    /// Returns `Ok(None)` if the block does not exist, or index is out of range.
    pub fn transaction_by_block_and_tx_index(
        &self,
        block_id: BlockId,
        index: usize,
    ) -> impl Future<
        Output = Result<Option<base_common_rpc_types::BaseTransaction>, BaseEthApiError>,
    > + Send {
        async move {
            if let Some(block) = self.recovered_block(block_id).await? {
                let block_hash = block.hash();
                let block_number = block.number();
                let block_timestamp = block.timestamp();
                let base_fee_per_gas = block.base_fee_per_gas();
                if let Some((signer, tx)) = block.transactions_with_sender().nth(index) {
                    let tx_info = TransactionInfo {
                        hash: Some(tx.tx_hash()),
                        block_hash: Some(block_hash),
                        block_number: Some(block_number),
                        block_timestamp: Some(block_timestamp),
                        base_fee: base_fee_per_gas,
                        index: Some(index as u64),
                    };

                    return Ok(Some(
                        self.converter().fill(tx.clone().with_signer(*signer), tx_info)?,
                    ));
                }
            }

            Ok(None)
        }
    }

    /// Find a transaction by sender's address and nonce.
    pub fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
        include_pending: bool,
    ) -> impl Future<
        Output = Result<Option<base_common_rpc_types::BaseTransaction>, BaseEthApiError>,
    > + Send {
        async move {
            // Check the pool first
            if include_pending
                && let Some(tx) = self.pool().get_transaction_by_sender_and_nonce(sender, nonce)
            {
                let transaction = tx.transaction.clone_into_consensus();
                return Ok(Some(self.converter().fill_pending(transaction)?));
            }

            // Note: we can't optimize for contracts (account with code) and cannot shortcircuit if
            // the address has code, because with 7702 EOAs can also have code

            let highest = self.transaction_count(sender, None).await?.saturating_to::<u64>();

            // If the nonce is higher or equal to the highest nonce, the transaction is pending or
            // not exists.
            if nonce >= highest {
                return Ok(None);
            }

            let high =
                self.provider().best_block_number().map_err(BaseEthApiError::from_eth_err)?;

            // Perform a binary search over the block range to find the block in which the sender's
            // nonce reached the requested nonce.
            let num = binary_search::<_, _, BaseEthApiError>(1, high, |mid| async move {
                let mid_nonce =
                    self.transaction_count(sender, Some(mid.into())).await?.saturating_to::<u64>();

                Ok(mid_nonce > nonce)
            })
            .await?;

            let block_id = num.into();
            self.recovered_block(block_id)
                .await?
                .and_then(|block| {
                    let block_hash = block.hash();
                    let block_number = block.number();
                    let block_timestamp = block.timestamp();
                    let base_fee_per_gas = block.base_fee_per_gas();

                    block
                        .transactions_with_sender()
                        .enumerate()
                        .find(|(_, (signer, tx))| **signer == sender && (*tx).nonce() == nonce)
                        .map(|(index, (signer, tx))| {
                            let tx_info = TransactionInfo {
                                hash: Some(tx.tx_hash()),
                                block_hash: Some(block_hash),
                                block_number: Some(block_number),
                                block_timestamp: Some(block_timestamp),
                                base_fee: base_fee_per_gas,
                                index: Some(index as u64),
                            };
                            Ok(self.converter().fill(tx.clone().with_signer(*signer), tx_info)?)
                        })
                })
                .ok_or(EthApiError::HeaderNotFound(block_id))?
                .map(Some)
        }
    }

    /// Get transaction, as raw bytes, by [`BlockId`] and index of transaction within that block.
    ///
    /// Returns `Ok(None)` if the block does not exist, or index is out of range.
    pub fn raw_transaction_by_block_and_tx_index(
        &self,
        block_id: BlockId,
        index: usize,
    ) -> impl Future<Output = Result<Option<Bytes>, BaseEthApiError>> + Send {
        async move {
            if let Some(block) = self.recovered_block(block_id).await?
                && let Some(tx) = block.body().transactions.get(index)
            {
                return Ok(Some(tx.encoded_2718().into()));
            }

            Ok(None)
        }
    }

    /// Signs transaction with a matching signer, if any and submits the transaction to the pool.
    /// Returns the hash of the signed transaction.
    pub fn send_transaction_request(
        &self,
        mut request: BaseTransactionRequest,
    ) -> impl Future<Output = Result<B256, BaseEthApiError>> + Send {
        async move {
            let from = match request.as_ref().from() {
                Some(from) => from,
                None => return Err(SignError::NoAccount.into()),
            };

            if self.find_signer(&from).is_err() {
                return Err(SignError::NoAccount.into());
            }

            // set nonce if not already set before
            if request.as_ref().nonce().is_none() {
                let nonce = self.next_available_nonce_for(&request).await?;
                request.as_mut().set_nonce(nonce);
            }

            let chain_id = self.chain_id();
            request.as_mut().set_chain_id(chain_id.to());

            let estimated_gas = self
                .estimate_gas_at(request.clone(), BlockId::pending(), EvmOverrides::default())
                .await?;
            let gas_limit = estimated_gas;
            request.as_mut().set_gas_limit(gas_limit.to());

            let transaction = self.sign_request(&from, request).await?.with_signer(from);

            let pool_transaction =
                base_execution_txpool::BasePooledTransaction::try_from_consensus(transaction)
                    .map_err(|e| {
                        BaseEthApiError::from_eth_err(TransactionConversionError::Other(
                            e.to_string(),
                        ))
                    })?;

            // submit the transaction to the pool with a `Local` origin
            let AddedTransactionOutcome { hash, .. } = self
                .pool()
                .add_transaction(TransactionOrigin::Local, pool_transaction)
                .await
                .map_err(BaseEthApiError::from_eth_err)?;

            Ok(hash)
        }
    }

    /// Fills the defaults on a given unsigned transaction.
    pub fn fill_transaction(
        &self,
        mut request: BaseTransactionRequest,
    ) -> impl Future<Output = Result<FillTransaction<BaseTxEnvelope>, BaseEthApiError>> + Send {
        async move {
            if request.as_ref().value().is_none() {
                request.as_mut().set_value(U256::ZERO);
            }

            if request.as_ref().nonce().is_none() {
                let nonce = self.next_available_nonce_for(&request).await?;
                request.as_mut().set_nonce(nonce);
            }

            let chain_id = self.chain_id();
            request.as_mut().set_chain_id(chain_id.to());

            if request.as_ref().has_eip4844_fields()
                && request.as_ref().max_fee_per_blob_gas().is_none()
            {
                let blob_fee = self.blob_base_fee().await?;
                request.as_mut().set_max_fee_per_blob_gas(blob_fee.to());
            }

            // Use `sidecar.is_some()` instead of `blob_sidecar().is_some()` to handle
            // both EIP-4844 (v0) and EIP-7594 (v1) sidecar formats
            if request.as_ref().sidecar.is_some()
                && request.as_ref().blob_versioned_hashes.is_none()
            {
                request.as_mut().populate_blob_hashes();
            }

            if request.as_ref().gas_limit().is_none() {
                let estimated_gas = self
                    .estimate_gas_at(request.clone(), BlockId::pending(), EvmOverrides::default())
                    .await?;
                request.as_mut().set_gas_limit(estimated_gas.to());
            }

            if request.as_ref().gas_price().is_none() {
                let tip = if let Some(tip) = request.as_ref().max_priority_fee_per_gas() {
                    tip
                } else {
                    let tip = self.suggested_priority_fee().await?.to::<u128>();
                    request.as_mut().set_max_priority_fee_per_gas(tip);
                    tip
                };
                if request.as_ref().max_fee_per_gas().is_none() {
                    let header =
                        self.provider().latest_header().map_err(BaseEthApiError::from_eth_err)?;
                    let base_fee = header.and_then(|h| h.base_fee_per_gas()).unwrap_or_default();
                    // Use `2 * base_fee` as headroom, matching go-ethereum's
                    // `setLondonFeeDefaults`, so the transaction does not
                    // become invalid if the base fee rises before it is
                    // included. This does not increase the effective price the sender pays:
                    // `max_fee_per_gas` is only an upper bound and the sender still pays
                    // `base_fee + min(tip, max_fee_per_gas - base_fee)`.
                    request.as_mut().set_max_fee_per_gas(base_fee as u128 * 2 + tip);
                }
            }

            let tx = self.converter().build_simulate_v1_transaction(request)?;

            let raw = tx.encoded_2718().into();

            Ok(FillTransaction { raw, tx })
        }
    }

    /// Signs a transaction, with configured signers.
    pub fn sign_request(
        &self,
        from: &Address,
        txn: BaseTransactionRequest,
    ) -> impl Future<Output = Result<ProviderTx<BlockchainProvider>, BaseEthApiError>> + Send {
        async move {
            self.find_signer(from)?
                .sign_transaction(txn, from)
                .await
                .map_err(BaseEthApiError::from_eth_err)
        }
    }

    /// Signs given message. Returns the signature.
    pub fn sign(
        &self,
        account: Address,
        message: Bytes,
    ) -> impl Future<Output = Result<Bytes, BaseEthApiError>> + Send {
        async move {
            Ok(self
                .find_signer(&account)?
                .sign(account, &message)
                .await
                .map_err(BaseEthApiError::from_eth_err)?
                .as_bytes()
                .into())
        }
    }

    /// Signs a transaction request using the given account in request
    /// Returns the EIP-2718 encoded signed transaction.
    pub fn sign_transaction(
        &self,
        request: BaseTransactionRequest,
    ) -> impl Future<Output = Result<Bytes, BaseEthApiError>> + Send {
        async move {
            let from = match request.as_ref().from() {
                Some(from) => from,
                None => return Err(SignError::NoAccount.into()),
            };

            Ok(self.sign_request(&from, request).await?.encoded_2718().into())
        }
    }

    /// Encodes and signs the typed data according EIP-712. Payload must implement Eip712 trait.
    pub fn sign_typed_data(
        &self,
        data: &TypedData,
        account: Address,
    ) -> Result<Bytes, BaseEthApiError> {
        Ok(self
            .find_signer(&account)?
            .sign_typed_data(account, data)
            .map_err(BaseEthApiError::from_eth_err)?
            .as_bytes()
            .into())
    }

    /// Returns the signer for the given account, if found in configured signers.
    #[expect(clippy::type_complexity)]
    pub fn find_signer(
        &self,
        account: &Address,
    ) -> Result<
        Box<dyn EthSigner<ProviderTx<BlockchainProvider>, BaseTransactionRequest> + 'static>,
        BaseEthApiError,
    > {
        self.signers()
            .read()
            .iter()
            .find(|signer| signer.is_signer_for(account))
            .map(|signer| dyn_clone::clone_box(&**signer))
            .ok_or_else(|| SignError::NoAccount.into())
    }
}

/// Loads a transaction from database.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` transactions RPC
/// methods.
impl BaseEthApi {
    /// Returns the transaction by including its corresponding [`BlockId`].
    ///
    /// Note: this supports pending transactions
    #[expect(clippy::type_complexity)]
    pub fn transaction_by_hash_at(
        &self,
        transaction_hash: B256,
    ) -> impl Future<
        Output = Result<
            Option<(TransactionSource<ProviderTx<BlockchainProvider>>, BlockId)>,
            BaseEthApiError,
        >,
    > + Send {
        async move {
            Ok(self.transaction_by_hash(transaction_hash).await?.map(|tx| match tx {
                tx @ TransactionSource::Pool(_) => (tx, BlockId::pending()),
                tx @ TransactionSource::Block { block_hash, .. } => {
                    (tx, BlockId::Hash(block_hash.into()))
                }
            }))
        }
    }

    /// Fetches the transaction and the transaction's block
    #[expect(clippy::type_complexity)]
    pub fn transaction_and_block(
        &self,
        hash: B256,
    ) -> impl Future<
        Output = Result<
            Option<(TransactionSource<ProviderTx<BlockchainProvider>>, Arc<RecoveredBlock>)>,
            BaseEthApiError,
        >,
    > + Send {
        async move {
            let (transaction, at) = match self.transaction_by_hash_at(hash).await? {
                None => return Ok(None),
                Some(res) => res,
            };

            // Note: this is always either hash or pending
            let block_hash = match at {
                BlockId::Hash(hash) => hash.block_hash,
                _ => return Ok(None),
            };
            let block = self
                .cache()
                .get_recovered_block(block_hash)
                .await
                .map_err(BaseEthApiError::from_eth_err)?;
            Ok(block.map(|block| (transaction, block)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, U256, map::AddressMap};
    use base_common_types_chain::{Block, Header, Transaction};
    use base_common_rpc_types::request::TransactionRequest;
    use base_execution_chainspec::BaseChainSpecBuilder;
    use base_execution_txpool::{
        TransactionOrigin, TransactionPool, test_utils::TransactionBuilder,
    };
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    use super::*;

    fn mock_eth_api(accounts: AddressMap<ExtendedAccount>) -> BaseEthApi {
        mock_eth_api_with_sync_timeout(accounts, Duration::from_secs(30))
    }

    fn mock_eth_api_with_sync_timeout(
        accounts: AddressMap<ExtendedAccount>,
        send_raw_transaction_sync_timeout: Duration,
    ) -> BaseEthApi {
        let mock_provider = MockEthProvider::default()
            .with_chain_spec(BaseChainSpecBuilder::base_mainnet().ecotone_activated().build());
        mock_provider.extend_accounts(accounts);
        let sender = base_execution_txpool::BasePooledTransaction::recover_raw_transaction(
            &raw_transfer_tx(),
        )
        .unwrap()
        .sender();
        mock_provider.add_account(sender, ExtendedAccount::new(0, U256::MAX));

        let genesis_header = Header {
            number: 0,
            gas_limit: 30_000_000,
            timestamp: 1,
            excess_blob_gas: Some(0),
            base_fee_per_gas: Some(1000000000),
            blob_gas_used: Some(0),
            ..Default::default()
        };

        let genesis_hash = genesis_header.hash_slow();
        mock_provider.add_block(genesis_hash, Block::new(genesis_header, Default::default()));
        mock_provider.add_receipts(0, Vec::new());

        crate::test_utils::RpcTestUtils::api_builder(mock_provider)
            .send_raw_transaction_sync_timeout(send_raw_transaction_sync_timeout)
            .build()
    }

    fn raw_transfer_tx_with_nonce(nonce: u64) -> Bytes {
        TransactionBuilder::default()
            .signer(B256::repeat_byte(1))
            .to(Address::repeat_byte(2))
            .chain_id(8453)
            .nonce(nonce)
            .gas_limit(21_000)
            .max_fee_per_gas(2_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .value(0)
            .into_eip1559()
            .encoded_2718()
            .into()
    }

    fn raw_transfer_tx() -> Bytes {
        raw_transfer_tx_with_nonce(0)
    }

    #[tokio::test]
    async fn send_raw_transaction() {
        let eth_api = mock_eth_api(Default::default());
        let pool = eth_api.pool();

        let tx_1 = raw_transfer_tx();

        let tx_1_result = eth_api.send_raw_transaction(tx_1).await.unwrap();
        assert_eq!(
            pool.pool_size().total,
            1,
            "expect 1 transaction in the pool, but pool size is {}",
            pool.pool_size().total
        );

        let tx_2 = raw_transfer_tx_with_nonce(1);

        let tx_2_result = eth_api.send_raw_transaction(tx_2).await.unwrap();
        assert_eq!(
            pool.pool_size().total,
            2,
            "expect 2 transactions in the pool, but pool size is {}",
            pool.pool_size().total
        );

        assert!(pool.get(&tx_1_result).is_some(), "tx1 not found in the pool");
        assert!(pool.get(&tx_2_result).is_some(), "tx2 not found in the pool");
        assert_eq!(pool.get(&tx_1_result).unwrap().origin, TransactionOrigin::Local);
        assert_eq!(pool.get(&tx_2_result).unwrap().origin, TransactionOrigin::Local);
    }

    #[tokio::test]
    async fn send_raw_transaction_sync_uses_request_timeout() {
        let eth_api = mock_eth_api(Default::default());

        let err = eth_api.send_raw_transaction_sync(raw_transfer_tx(), Some(1)).await.unwrap_err();

        assert!(matches!(
            err,
            reth_rpc_eth_types::BaseEthApiError::Eth(EthApiError::TransactionConfirmationTimeout { duration, .. })
                if duration == Duration::from_millis(1)
        ));
        assert_eq!(eth_api.pool().pool_size().total, 1);
    }

    #[tokio::test]
    async fn send_raw_transaction_sync_uses_configured_timeout_when_omitted() {
        let eth_api = mock_eth_api_with_sync_timeout(Default::default(), Duration::from_millis(1));

        let err = eth_api.send_raw_transaction_sync(raw_transfer_tx(), None).await.unwrap_err();

        assert!(matches!(
            err,
            reth_rpc_eth_types::BaseEthApiError::Eth(EthApiError::TransactionConfirmationTimeout { duration, .. })
                if duration == Duration::from_millis(1)
        ));
        assert_eq!(eth_api.pool().pool_size().total, 1);
    }

    #[tokio::test]
    async fn send_raw_transaction_sync_uses_configured_timeout_when_zero() {
        let eth_api = mock_eth_api_with_sync_timeout(Default::default(), Duration::from_millis(1));

        let err = eth_api.send_raw_transaction_sync(raw_transfer_tx(), Some(0)).await.unwrap_err();

        assert!(matches!(
            err,
            reth_rpc_eth_types::BaseEthApiError::Eth(EthApiError::TransactionConfirmationTimeout { duration, .. })
                if duration == Duration::from_millis(1)
        ));
        assert_eq!(eth_api.pool().pool_size().total, 1);
    }

    #[tokio::test]
    async fn send_raw_transaction_sync_caps_request_timeout() {
        let eth_api = mock_eth_api_with_sync_timeout(Default::default(), Duration::from_millis(1));

        let err = eth_api.send_raw_transaction_sync(raw_transfer_tx(), Some(50)).await.unwrap_err();

        assert!(matches!(
            err,
            reth_rpc_eth_types::BaseEthApiError::Eth(EthApiError::TransactionConfirmationTimeout { duration, .. })
                if duration == Duration::from_millis(1)
        ));
        assert_eq!(eth_api.pool().pool_size().total, 1);
    }

    #[tokio::test]
    async fn test_fill_transaction_fills_chain_id() {
        let address = Address::random();
        let accounts = AddressMap::from_iter([(
            address,
            ExtendedAccount::new(0, U256::from(10_000_000_000_000_000_000u64)), // 10 ETH
        )]);

        let eth_api = mock_eth_api(accounts);

        let tx_req = TransactionRequest {
            from: Some(address),
            to: Some(Address::random().into()),
            gas: Some(21_000),
            ..Default::default()
        };

        let filled =
            eth_api.fill_transaction(tx_req.into()).await.expect("fill_transaction should succeed");

        // Should fill with the chain id from provider
        assert!(filled.tx.chain_id().is_some());
    }

    #[tokio::test]
    async fn test_fill_transaction_fills_nonce() {
        let address = Address::random();
        let nonce = 42u64;

        let accounts = AddressMap::from_iter([(
            address,
            ExtendedAccount::new(nonce, U256::from(1_000_000_000_000_000_000u64)), // 1 ETH
        )]);

        let eth_api = mock_eth_api(accounts);

        let tx_req = TransactionRequest {
            from: Some(address),
            to: Some(Address::random().into()),
            value: Some(U256::from(1000)),
            gas: Some(21_000),
            ..Default::default()
        };

        let filled =
            eth_api.fill_transaction(tx_req.into()).await.expect("fill_transaction should succeed");

        assert_eq!(filled.tx.nonce(), nonce);
    }

    #[tokio::test]
    async fn test_fill_transaction_preserves_provided_fields() {
        let address = Address::random();
        let provided_nonce = 100u64;
        let provided_gas_limit = 50_000u64;

        let accounts = AddressMap::from_iter([(
            address,
            ExtendedAccount::new(42, U256::from(10_000_000_000_000_000_000u64)),
        )]);

        let eth_api = mock_eth_api(accounts);

        let tx_req = TransactionRequest {
            from: Some(address),
            to: Some(Address::random().into()),
            value: Some(U256::from(1000)),
            nonce: Some(provided_nonce),
            gas: Some(provided_gas_limit),
            ..Default::default()
        };

        let filled =
            eth_api.fill_transaction(tx_req.into()).await.expect("fill_transaction should succeed");

        // Should preserve the provided nonce and gas limit
        assert_eq!(filled.tx.nonce(), provided_nonce);
        assert_eq!(filled.tx.gas_limit(), provided_gas_limit);
    }

    #[tokio::test]
    async fn test_fill_transaction_fills_all_missing_fields() {
        let address = Address::random();

        let balance = U256::from(100u128) * U256::from(1_000_000_000_000_000_000u128);
        let accounts = AddressMap::from_iter([(address, ExtendedAccount::new(5, balance))]);

        let eth_api = mock_eth_api(accounts);

        // Create a simple transfer transaction
        let tx_req = TransactionRequest {
            from: Some(address),
            to: Some(Address::random().into()),
            ..Default::default()
        };

        let filled =
            eth_api.fill_transaction(tx_req.into()).await.expect("fill_transaction should succeed");

        assert!(filled.tx.is_eip1559());
    }

    #[tokio::test]
    async fn test_fill_transaction_non_blob_tx_no_blob_fee() {
        let address = Address::random();
        let accounts = AddressMap::from_iter([(
            address,
            ExtendedAccount::new(0, U256::from(10_000_000_000_000_000_000u64)),
        )]);

        let eth_api = mock_eth_api(accounts);

        // EIP-1559 transaction without blob fields
        let tx_req = TransactionRequest {
            from: Some(address),
            to: Some(Address::random().into()),
            transaction_type: Some(2), // EIP-1559
            ..Default::default()
        };

        let filled =
            eth_api.fill_transaction(tx_req.into()).await.expect("fill_transaction should succeed");

        // Non-blob transaction should NOT have blob fee filled
        assert!(
            filled.tx.max_fee_per_blob_gas().is_none(),
            "max_fee_per_blob_gas should not be set for non-blob tx"
        );
    }
}
