//! Merged best-transaction iteration across the protocol pool and 2D nonce sidecar.

use std::{cmp::Reverse, sync::Arc};

use reth_transaction_pool::{
    BestTransactions, TransactionOrdering, ValidPoolTransaction, error::InvalidPoolTransactionError,
};

use crate::BasePooledTx;

/// Merges best-transaction iterators from the protocol pool and the 2D nonce sidecar.
pub(crate) struct MergeBestTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    protocol: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
    sidecar: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
    ordering: O,
    base_fee: u64,
    next_protocol: Option<Arc<ValidPoolTransaction<T>>>,
    next_sidecar: Option<Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx, O> MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    /// Creates a merged iterator from the protocol pool and 2D nonce sidecar.
    pub(crate) fn new(
        protocol: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
        sidecar: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
        ordering: O,
        base_fee: u64,
    ) -> Self {
        Self { protocol, sidecar, ordering, base_fee, next_protocol: None, next_sidecar: None }
    }

    fn protocol_is_better(
        &self,
        protocol: &Arc<ValidPoolTransaction<T>>,
        sidecar: &Arc<ValidPoolTransaction<T>>,
    ) -> bool {
        let protocol_priority = (
            self.ordering.priority(&protocol.transaction, self.base_fee),
            Reverse(protocol.timestamp),
            *protocol.hash(),
        );
        let sidecar_priority = (
            self.ordering.priority(&sidecar.transaction, self.base_fee),
            Reverse(sidecar.timestamp),
            *sidecar.hash(),
        );
        protocol_priority >= sidecar_priority
    }

    fn pop_best(&mut self) -> Option<Arc<ValidPoolTransaction<T>>> {
        match (&self.next_protocol, &self.next_sidecar) {
            (Some(protocol), Some(sidecar)) => {
                if self.protocol_is_better(protocol, sidecar) {
                    self.next_protocol.take()
                } else {
                    self.next_sidecar.take()
                }
            }
            (Some(_), None) => self.next_protocol.take(),
            (None, Some(_)) => self.next_sidecar.take(),
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
            if self.next_protocol.is_none() {
                self.next_protocol = self.protocol.next();
            }
            if self.next_sidecar.is_none() {
                self.next_sidecar = self.sidecar.next();
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
        if transaction.transaction.eip8130_nonce_channel_key().is_some() {
            self.next_sidecar = None;
            self.sidecar.mark_invalid(transaction, kind);
        } else {
            self.next_protocol = None;
            self.protocol.mark_invalid(transaction, kind);
        }
    }

    fn no_updates(&mut self) {
        self.protocol.no_updates();
        self.sidecar.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.protocol.set_skip_blobs(skip_blobs);
        self.sidecar.set_skip_blobs(skip_blobs);
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
            expiry: 0,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
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
