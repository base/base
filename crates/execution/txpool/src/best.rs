//! Merged best-transaction iteration across two transaction stores.

use std::sync::Arc;

use reth_transaction_pool::{
    BestTransactions, TransactionOrdering, ValidPoolTransaction, error::InvalidPoolTransactionError,
};

use crate::{BasePooledTx, BestTransactionPriority};

/// Merges best-transaction iterators from two stores by complete priority.
///
/// Each yielded transaction belongs to exactly one source, decided by `secondary_owns`.
/// Invalidations are routed only to the owning source, so a source-local sender id is never
/// invalidated in the other store.
pub(crate) struct MergeBestTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    primary: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
    secondary: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
    secondary_owns: fn(&T) -> bool,
    ordering: O,
    base_fee: u64,
    next_primary: Option<Arc<ValidPoolTransaction<T>>>,
    next_secondary: Option<Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx, O> MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    /// Creates a merged iterator. `secondary_owns` returns whether a transaction was yielded by
    /// `secondary`; every other transaction belongs to `primary`. The primary source wins ties.
    pub(crate) fn new(
        primary: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
        secondary: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
        secondary_owns: fn(&T) -> bool,
        ordering: O,
        base_fee: u64,
    ) -> Self {
        Self {
            primary,
            secondary,
            secondary_owns,
            ordering,
            base_fee,
            next_primary: None,
            next_secondary: None,
        }
    }

    fn primary_is_better(
        &self,
        primary: &Arc<ValidPoolTransaction<T>>,
        secondary: &Arc<ValidPoolTransaction<T>>,
    ) -> bool {
        let primary_priority = BestTransactionPriority::new(&self.ordering, primary, self.base_fee);
        let secondary_priority =
            BestTransactionPriority::new(&self.ordering, secondary, self.base_fee);
        primary_priority >= secondary_priority
    }

    fn pop_best(&mut self) -> Option<Arc<ValidPoolTransaction<T>>> {
        match (&self.next_primary, &self.next_secondary) {
            (Some(primary), Some(secondary)) => {
                if self.primary_is_better(primary, secondary) {
                    self.next_primary.take()
                } else {
                    self.next_secondary.take()
                }
            }
            (Some(_), None) => self.next_primary.take(),
            (None, Some(_)) => self.next_secondary.take(),
            (None, None) => None,
        }
    }
}

impl<T: BasePooledTx, O> std::fmt::Debug for MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeBestTransactions").finish_non_exhaustive()
    }
}

impl<T: BasePooledTx, O> Iterator for MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.next_primary.is_none() {
                self.next_primary = self.primary.next();
            }
            if self.next_secondary.is_none() {
                self.next_secondary = self.secondary.next();
            }

            let transaction = self.pop_best()?;
            if transaction.effective_tip_per_gas(self.base_fee).is_some() {
                return Some(transaction);
            }

            self.mark_invalid(&transaction, InvalidPoolTransactionError::Underpriced);
        }
    }
}

impl<T: BasePooledTx, O> BestTransactions for MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        if (self.secondary_owns)(&transaction.transaction) {
            self.next_secondary = None;
            self.secondary.mark_invalid(transaction, kind);
        } else {
            self.next_primary = None;
            self.primary.mark_invalid(transaction, kind);
        }
    }

    fn no_updates(&mut self) {
        self.primary.no_updates();
        self.secondary.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.primary.set_skip_blobs(skip_blobs);
        self.secondary.set_skip_blobs(skip_blobs);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::Instant};

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Signed, TxEip8130,
    };
    use reth_transaction_pool::{TransactionOrigin, identifier::TransactionId};

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction, TimestampedTransaction};

    #[derive(Debug)]
    struct StaticBestTransactions<T: BasePooledTx> {
        transactions: VecDeque<Arc<ValidPoolTransaction<T>>>,
    }

    impl<T: BasePooledTx> StaticBestTransactions<T> {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<T>>>) -> Self {
            Self { transactions: transactions.into() }
        }
    }

    impl<T: BasePooledTx> Iterator for StaticBestTransactions<T> {
        type Item = Arc<ValidPoolTransaction<T>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.transactions.pop_front()
        }
    }

    impl<T: BasePooledTx> BestTransactions for StaticBestTransactions<T> {
        fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
            let nonce_key = transaction.transaction.eip8130_nonce_channel_key();
            self.transactions.retain(|candidate| {
                if nonce_key.is_some() {
                    candidate.sender() != transaction.sender()
                        || candidate.transaction.eip8130_nonce_channel_key() != nonce_key
                } else {
                    candidate.sender() != transaction.sender()
                }
            });
        }

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn signed_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_tx_with_received_at(
            signer,
            nonce_key,
            nonce_sequence,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            0,
        )
    }

    fn signed_tx_with_received_at(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
        received_at: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        let encoded_length = pooled.encode_2718_len();
        BasePooledTransaction::new_with_received_at(
            Recovered::new_unchecked(pooled.into(), signer.address()),
            encoded_length,
            received_at,
        )
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        valid_pool_transaction_at(transaction, Instant::now())
    }

    fn valid_pool_transaction_at(
        transaction: BasePooledTransaction,
        timestamp: Instant,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp,
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    #[test]
    fn prefers_higher_effective_tip_across_protocol_and_sidecar() {
        let protocol = valid_pool_transaction(signed_tx(&signer(), U256::ZERO, 0, 1, 100));
        let sidecar = valid_pool_transaction(signed_tx(&signer(), U256::from(1), 0, 50, 50));
        let sidecar_hash = *sidecar.hash();

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![protocol])),
            Box::new(StaticBestTransactions::new(vec![sidecar])),
            BasePooledTransaction::is_eip8130_sidecar_transaction,
            BaseOrdering::coinbase_tip(),
            10,
        );

        assert_eq!(merged.next().map(|transaction| *transaction.hash()), Some(sidecar_hash));
    }

    #[test]
    fn skips_underpriced_sidecar_transactions_for_current_base_fee() {
        let sidecar = valid_pool_transaction(signed_tx(&signer(), U256::from(7), 0, 5, 5));

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(Vec::new())),
            Box::new(StaticBestTransactions::new(vec![sidecar])),
            BasePooledTransaction::is_eip8130_sidecar_transaction,
            BaseOrdering::coinbase_tip(),
            10,
        );

        assert!(merged.next().is_none());
    }

    #[test]
    fn respects_timestamp_ordering_across_protocol_and_sidecar() {
        let older_protocol =
            valid_pool_transaction(signed_tx_with_received_at(&signer(), U256::ZERO, 0, 1, 100, 1));
        let newer_sidecar = valid_pool_transaction(signed_tx_with_received_at(
            &signer(),
            U256::from(9),
            0,
            50,
            50,
            2,
        ));

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![Arc::clone(&older_protocol)])),
            Box::new(StaticBestTransactions::new(vec![newer_sidecar])),
            BasePooledTransaction::is_eip8130_sidecar_transaction,
            BaseOrdering::timestamp(),
            10,
        );

        let first = merged.next().expect("expected a merged transaction");
        assert_eq!(first.transaction.received_at(), older_protocol.transaction.received_at());
    }

    #[test]
    fn equal_priority_prefers_earlier_submission_timestamp() {
        let now = Instant::now();
        let older_protocol =
            valid_pool_transaction_at(signed_tx(&signer(), U256::ZERO, 0, 10, 100), now);
        let newer_sidecar = valid_pool_transaction_at(
            signed_tx(&signer(), U256::from(17), 0, 10, 100),
            now + std::time::Duration::from_secs(1),
        );
        let older_hash = *older_protocol.hash();

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![older_protocol])),
            Box::new(StaticBestTransactions::new(vec![newer_sidecar])),
            BasePooledTransaction::is_eip8130_sidecar_transaction,
            BaseOrdering::coinbase_tip(),
            10,
        );

        assert_eq!(merged.next().map(|transaction| *transaction.hash()), Some(older_hash));
    }

    #[test]
    fn mark_invalid_clears_cached_protocol_transaction() {
        let signer = signer();
        let protocol = vec![
            valid_pool_transaction(signed_tx(&signer, U256::ZERO, 0, 1_200, 1_200)),
            valid_pool_transaction(signed_tx(&signer, U256::ZERO, 1, 900, 900)),
        ];
        let sidecar =
            vec![valid_pool_transaction(signed_tx(&signer, U256::from(1), 0, 1_000, 1_000))];

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(protocol)),
            Box::new(StaticBestTransactions::new(sidecar)),
            BasePooledTransaction::is_eip8130_sidecar_transaction,
            BaseOrdering::coinbase_tip(),
            0,
        );

        let first = merged.next().expect("expected protocol transaction");
        assert_eq!(first.transaction.eip8130_nonce_channel_key(), None);
        assert_eq!(first.nonce(), 0);

        let second = merged.next().expect("expected sidecar transaction");
        assert_eq!(second.transaction.eip8130_nonce_channel_key(), Some(U256::from(1)));

        merged.mark_invalid(&first, InvalidPoolTransactionError::Underpriced);

        assert!(merged.next().is_none());
    }
}
