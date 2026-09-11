//! A transaction pool implementation that does nothing.
//!
//! This is useful for wiring components together that don't require an actual pool but still need
//! to be generic over it.

use std::sync::Arc;

use alloy_eips::eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M;
use alloy_primitives::{Address, TxHash, U256, map::AddressSet};
use base_common_types_chain::Recovered;
use base_execution_network_wire::HandleMempoolData;
use tokio::sync::{mpsc, mpsc::Receiver};

use crate::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BlockInfo, NewTransactionEvent, PoolResult, PoolSize, PropagatedTransactions,
    TransactionEvents, TransactionOrigin, TransactionPool, TransactionValidationOutcome,
    TransactionValidator, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError},
    pool::TransactionListenerKind,
    traits::{BestTransactionsAttributes, GetPooledTransactionLimit},
};

/// A [`TransactionPool`] implementation that does nothing.
///
/// All transactions are rejected and no events are emitted.
/// This type will never hold any transactions and is only useful for wiring components together.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NoopTransactionPool {}

impl NoopTransactionPool {
    /// Creates a new [`NoopTransactionPool`].
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for NoopTransactionPool {
    fn default() -> Self {
        Self {}
    }
}

impl crate::ParkableTransactionPool for NoopTransactionPool {
    fn best_transactions_with_attributes_and_parking(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> Box<dyn crate::ParkableBestTransactions> {
        Box::new(crate::ParkedBestTransactions::new(
            self.best_transactions(),
            crate::BaseOrdering::default(),
            attributes.basefee,
        ))
    }
}

impl TransactionPool for NoopTransactionPool {
    fn pool_size(&self) -> PoolSize {
        Default::default()
    }

    fn block_info(&self) -> BlockInfo {
        BlockInfo {
            block_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
            last_seen_block_hash: Default::default(),
            last_seen_block_number: 0,
            pending_basefee: 0,
            pending_blob_fee: None,
        }
    }

    async fn add_transaction_and_subscribe(
        &self,
        _origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> PoolResult<TransactionEvents> {
        let hash = *transaction.hash();
        Err(PoolError::other(hash, Box::new(NoopInsertError::new(transaction))))
    }

    async fn add_transaction(
        &self,
        _origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        let hash = *transaction.hash();
        Err(PoolError::other(hash, Box::new(NoopInsertError::new(transaction))))
    }

    async fn add_transactions(
        &self,
        _origin: TransactionOrigin,
        transactions: Vec<crate::BasePooledTransaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        transactions
            .into_iter()
            .map(|transaction| {
                let hash = *transaction.hash();
                Err(PoolError::other(hash, Box::new(NoopInsertError::new(transaction))))
            })
            .collect()
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, crate::BasePooledTransaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        transactions
            .into_iter()
            .map(|(_, transaction)| {
                let hash = *transaction.hash();
                Err(PoolError::other(hash, Box::new(NoopInsertError::new(transaction))))
            })
            .collect()
    }

    fn transaction_event_listener(&self, _tx_hash: TxHash) -> Option<TransactionEvents> {
        None
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents {
        AllTransactionsEvents::new(mpsc::channel(1).1)
    }

    fn pending_transactions_listener_for(
        &self,
        _kind: TransactionListenerKind,
    ) -> Receiver<TxHash> {
        mpsc::channel(1).1
    }

    fn new_transactions_listener(&self) -> Receiver<NewTransactionEvent> {
        mpsc::channel(1).1
    }

    fn new_transactions_listener_for(
        &self,
        _kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent> {
        mpsc::channel(1).1
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        vec![]
    }

    fn pooled_transaction_hashes_max(&self, _max: usize) -> Vec<TxHash> {
        vec![]
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn pooled_transactions_max(&self, _max: usize) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn get_pooled_transaction_elements(
        &self,
        _tx_hashes: Vec<TxHash>,
        _limit: GetPooledTransactionLimit,
    ) -> Vec<base_common_types_chain::BasePooledTransaction> {
        vec![]
    }

    fn append_pooled_transaction_elements(
        &self,
        _tx_hashes: &[TxHash],
        _limit: GetPooledTransactionLimit,
        _out: &mut Vec<base_common_types_chain::BasePooledTransaction>,
    ) {
    }

    fn get_pooled_transaction_element(
        &self,
        _tx_hash: TxHash,
    ) -> Option<Recovered<base_common_types_chain::BasePooledTransaction>> {
        None
    }

    fn best_transactions(&self) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction>>> {
        Box::new(std::iter::empty())
    }

    fn best_transactions_with_attributes(
        &self,
        _: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction>>> {
        Box::new(std::iter::empty())
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn pending_transactions_max(&self, _max: usize) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        (0, 0)
    }

    fn all_transactions(&self) -> AllPoolTransactions {
        AllPoolTransactions::default()
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        vec![]
    }

    fn remove_transactions(&self, _hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn remove_transactions_and_descendants(
        &self,
        _hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn remove_transactions_by_sender(&self, _sender: Address) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn prune_transactions(&self, _hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn retain_unknown<A>(&self, _announcement: &mut A)
    where
        A: HandleMempoolData,
    {
    }

    fn retain_contains<A>(&self, _announcement: &mut A)
    where
        A: HandleMempoolData,
    {
    }

    fn get(&self, _tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction>> {
        None
    }

    fn get_all(&self, _txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn on_propagated(&self, _txs: PropagatedTransactions) {}

    fn get_transactions_by_sender(&self, _sender: Address) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn get_pending_transactions_with_predicate(
        &self,
        _predicate: impl FnMut(&ValidPoolTransaction) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn get_pending_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn get_queued_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn get_highest_transaction_by_sender(
        &self,
        _sender: Address,
    ) -> Option<Arc<ValidPoolTransaction>> {
        None
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        _sender: Address,
        _on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>> {
        None
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        _sender: Address,
        _nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>> {
        None
    }

    fn get_transactions_by_origin(
        &self,
        _origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn get_pending_transactions_by_origin(
        &self,
        _origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        vec![]
    }

    fn unique_senders(&self) -> AddressSet {
        Default::default()
    }
}

/// A [`TransactionValidator`] that does nothing.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MockTransactionValidator {
    propagate_local: bool,
    return_invalid: bool,
}

impl TransactionValidator for MockTransactionValidator {
    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> TransactionValidationOutcome {
        if self.return_invalid {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::Underpriced,
            );
        }
        // we return `balance: U256::MAX` to simulate a valid transaction which will never go into
        // overdraft
        TransactionValidationOutcome::Valid {
            balance: U256::MAX,
            state_nonce: 0,
            bytecode_hash: None,
            transaction,
            propagate: match origin {
                TransactionOrigin::External => true,
                TransactionOrigin::Local => self.propagate_local,
                TransactionOrigin::Private => false,
            },
            authorities: None,
        }
    }
}

impl MockTransactionValidator {
    /// Creates a new [`MockTransactionValidator`] that does not allow local transactions to be
    /// propagated.
    pub fn no_propagate_local() -> Self {
        Self { propagate_local: false, return_invalid: false }
    }
    /// Creates a new [`MockTransactionValidator`] that always returns an invalid outcome.
    pub fn return_invalid() -> Self {
        Self { propagate_local: false, return_invalid: true }
    }
}

impl Default for MockTransactionValidator {
    fn default() -> Self {
        Self { propagate_local: true, return_invalid: false }
    }
}

/// An error that contains the transaction that failed to be inserted into the noop pool.
#[derive(Debug, Clone, thiserror::Error)]
#[error("can't insert transaction into the noop pool that does nothing")]
pub struct NoopInsertError {
    tx: crate::BasePooledTransaction,
}

impl NoopInsertError {
    const fn new(tx: crate::BasePooledTransaction) -> Self {
        Self { tx }
    }

    /// Returns the transaction that failed to be inserted.
    pub fn into_inner(self) -> crate::BasePooledTransaction {
        self.tx
    }
}
