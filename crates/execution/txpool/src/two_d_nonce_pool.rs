use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use alloy_primitives::{Address, TxHash, U256};
use base_common_consensus::Eip8130Constants;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolResult, PriceBumpConfig, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind},
    identifier::{SenderIdentifiers, TransactionId},
    pool::{AddedTransactionState, QueuedReason},
};

use crate::BasePooledTx;

type LaneId = (Address, U256);

#[derive(Debug)]
struct NonceLane<T: BasePooledTx> {
    next_nonce: u64,
    transactions: BTreeMap<u64, Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx> Default for NonceLane<T> {
    fn default() -> Self {
        Self { next_nonce: 0, transactions: BTreeMap::new() }
    }
}

/// Outcome returned after inserting into the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct InsertOutcome<T: BasePooledTx> {
    pub outcome: AddedTransactionOutcome,
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
}

/// Minimal 2D nonce sidecar for finite non-zero `nonce_key` channels.
#[derive(Debug)]
pub(crate) struct TwoDNoncePool<T: BasePooledTx> {
    lanes: HashMap<LaneId, NonceLane<T>>,
    hashes: HashMap<TxHash, Arc<ValidPoolTransaction<T>>>,
    index: HashMap<TxHash, (LaneId, u64)>,
    senders: SenderIdentifiers,
    price_bump_config: PriceBumpConfig,
}

impl<T: BasePooledTx> TwoDNoncePool<T> {
    /// Creates a new 2D nonce sidecar pool.
    pub(crate) fn new(price_bump_config: PriceBumpConfig) -> Self {
        Self {
            lanes: HashMap::new(),
            hashes: HashMap::new(),
            index: HashMap::new(),
            senders: SenderIdentifiers::default(),
            price_bump_config,
        }
    }

    /// Returns true if the sidecar already contains the hash.
    pub(crate) fn contains(&self, hash: &TxHash) -> bool {
        self.hashes.contains_key(hash)
    }

    /// Returns the number of pending and queued transactions.
    pub(crate) fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let mut pending = 0;
        let mut queued = 0;
        for lane in self.lanes.values() {
            for nonce in lane.transactions.keys() {
                if *nonce == lane.next_nonce {
                    pending += 1;
                } else {
                    queued += 1;
                }
            }
        }
        (pending, queued)
    }

    /// Returns all pending transactions.
    pub(crate) fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.lanes
            .values()
            .filter_map(|lane| lane.transactions.get(&lane.next_nonce).cloned())
            .collect()
    }

    /// Returns all queued transactions.
    pub(crate) fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            for (nonce, transaction) in &lane.transactions {
                if *nonce != lane.next_nonce {
                    transactions.push(Arc::clone(transaction));
                }
            }
        }
        transactions
    }

    /// Returns all transactions in the sidecar.
    pub(crate) fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.hashes.values().cloned().collect()
    }

    /// Returns all transaction hashes in the sidecar.
    pub(crate) fn all_hashes(&self) -> Vec<TxHash> {
        self.hashes.keys().copied().collect()
    }

    /// Returns the transaction for the given hash.
    pub(crate) fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.hashes.get(hash).cloned()
    }

    /// Returns transactions for the given sender.
    pub(crate) fn transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.hashes.values().filter(|tx| tx.sender() == sender).cloned().collect()
    }

    /// Returns pending transactions for the given sender.
    pub(crate) fn pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions().into_iter().filter(|tx| tx.sender() == sender).collect()
    }

    /// Returns queued transactions for the given sender.
    pub(crate) fn queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.queued_transactions().into_iter().filter(|tx| tx.sender() == sender).collect()
    }

    /// Returns the highest transaction for the sender across all nonce channels.
    pub(crate) fn highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.transactions_by_sender(sender).into_iter().max_by_key(|tx| tx.nonce())
    }

    /// Returns the highest pending transaction for the sender.
    pub(crate) fn highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions_by_sender(sender).into_iter().max_by_key(|tx| tx.nonce())
    }

    /// Returns the first transaction that matches the sender and nonce sequence.
    pub(crate) fn transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.transactions_by_sender(sender).into_iter().find(|tx| tx.nonce() == nonce)
    }

    /// Returns all senders present in the sidecar.
    pub(crate) fn unique_senders(&self) -> HashSet<Address> {
        self.hashes.values().map(|tx| tx.sender()).collect()
    }

    /// Returns or creates the sender id for the given address.
    pub(crate) fn sender_id_or_create(
        &mut self,
        address: Address,
    ) -> reth_transaction_pool::identifier::SenderId {
        self.senders.sender_id_or_create(address)
    }

    /// Inserts a validated channelized EIP-8130 transaction.
    pub(crate) fn insert_validated(
        &mut self,
        mut transaction: ValidPoolTransaction<T>,
    ) -> PoolResult<InsertOutcome<T>> {
        let hash = *transaction.hash();
        if self.contains(&hash) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }

        let sender = transaction.sender();
        let nonce_key = transaction
            .transaction
            .eip8130_nonce_channel_key()
            .expect("2D nonce pool only accepts channelized EIP-8130 transactions");
        debug_assert!(nonce_key != U256::ZERO && nonce_key != Eip8130Constants::NONCE_KEY_MAX);

        let lane_id = (sender, nonce_key);
        let sender_id = self.senders.sender_id_or_create(sender);
        let nonce = transaction.nonce();
        transaction.transaction_id = TransactionId::new(sender_id, nonce);
        let transaction = Arc::new(transaction);
        let lane = self.lanes.entry(lane_id).or_default();

        if nonce < lane.next_nonce {
            return Err(PoolError::new(
                hash,
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::NonceNotConsistent {
                        tx: nonce,
                        state: lane.next_nonce,
                    },
                )),
            ));
        }

        let replaced: Option<Arc<ValidPoolTransaction<T>>> =
            if let Some(existing) = lane.transactions.get(&nonce) {
                if existing.is_underpriced(&transaction, &self.price_bump_config) {
                    return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
                }
                Some(Arc::clone(existing))
            } else {
                None
            };

        lane.transactions.insert(nonce, Arc::clone(&transaction));
        self.hashes.insert(hash, Arc::clone(&transaction));
        self.index.insert(hash, (lane_id, nonce));

        if let Some(replaced) = &replaced {
            let replaced_hash = *replaced.hash();
            self.hashes.remove(&replaced_hash);
            self.index.remove(&replaced_hash);
        }

        let state = if nonce == lane.next_nonce {
            AddedTransactionState::Pending
        } else {
            AddedTransactionState::Queued(QueuedReason::NonceGap)
        };

        Ok(InsertOutcome { outcome: AddedTransactionOutcome { hash, state }, replaced })
    }

    /// Removes the exact transactions by hash without advancing lane state.
    pub(crate) fn remove_transactions(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            if let Some(transaction) = self.remove_hash(hash, false) {
                removed.push(transaction);
            }
        }
        removed
    }

    /// Removes transactions and their descendants for each hash.
    pub(crate) fn remove_transactions_and_descendants(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            let Some((lane_id, nonce)) = self.index.get(hash).copied() else {
                continue;
            };
            let Some(lane) = self.lanes.get(&lane_id) else {
                continue;
            };

            let descendant_hashes: Vec<_> = lane
                .transactions
                .range(nonce..)
                .map(|(_, transaction)| *transaction.hash())
                .collect();
            removed.extend(self.remove_transactions(&descendant_hashes));
        }
        removed
    }

    /// Prunes mined transactions and advances the matching lane heads.
    pub(crate) fn prune_mined(&mut self, hashes: &[TxHash]) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            if let Some(transaction) = self.remove_hash(hash, true) {
                removed.push(transaction);
            }
        }
        removed
    }

    /// Removes all transactions for the given sender.
    pub(crate) fn remove_transactions_by_sender(
        &mut self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let hashes: Vec<_> =
            self.hashes.values().filter(|tx| tx.sender() == sender).map(|tx| *tx.hash()).collect();
        self.remove_transactions(&hashes)
    }

    /// Returns a best-transactions iterator snapshot.
    pub(crate) fn best_transactions(&self) -> BestTwoDTransactions<T> {
        BestTwoDTransactions::new(&self.lanes)
    }

    fn remove_hash(
        &mut self,
        hash: &TxHash,
        advance_lane: bool,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let ((sender, nonce_key), nonce) = self.index.remove(hash)?;
        let lane_id = (sender, nonce_key);
        let transaction = {
            let lane = self.lanes.get_mut(&lane_id)?;
            let transaction = lane.transactions.remove(&nonce)?;
            if advance_lane && nonce == lane.next_nonce {
                lane.next_nonce += 1;
            }
            transaction
        };

        if self.lanes.get(&lane_id).is_some_and(|lane| lane.transactions.is_empty()) {
            self.lanes.remove(&lane_id);
        }
        self.hashes.remove(hash);
        Some(transaction)
    }
}

/// Snapshot iterator over the current best transactions of the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct BestTwoDTransactions<T: BasePooledTx> {
    lanes: Vec<LaneIterator<T>>,
}

#[derive(Debug)]
struct LaneIterator<T: BasePooledTx> {
    id: LaneId,
    transactions: Vec<Arc<ValidPoolTransaction<T>>>,
    index: usize,
    invalidated: bool,
}

impl<T: BasePooledTx> BestTwoDTransactions<T> {
    fn new(lanes: &HashMap<LaneId, NonceLane<T>>) -> Self {
        let lanes = lanes
            .iter()
            .filter_map(|(id, lane)| {
                let mut next_nonce = lane.next_nonce;
                let mut transactions = Vec::new();
                while let Some(transaction) = lane.transactions.get(&next_nonce) {
                    transactions.push(Arc::clone(transaction));
                    next_nonce += 1;
                }
                (!transactions.is_empty()).then(|| LaneIterator {
                    id: *id,
                    transactions,
                    index: 0,
                    invalidated: false,
                })
            })
            .collect();
        Self { lanes }
    }
}

impl<T: BasePooledTx> Iterator for BestTwoDTransactions<T> {
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        let best_index = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| {
                (!lane.invalidated && lane.index < lane.transactions.len())
                    .then_some((index, lane.transactions[lane.index].transaction.max_fee_per_gas()))
            })
            .max_by_key(|(_, fee)| *fee)
            .map(|(index, _)| index)?;

        let lane = &mut self.lanes[best_index];
        let transaction = Arc::clone(&lane.transactions[lane.index]);
        lane.index += 1;
        Some(transaction)
    }
}

impl<T: BasePooledTx> BestTransactions for BestTwoDTransactions<T> {
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: &InvalidPoolTransactionError) {
        if let Some(lane) = self.lanes.iter_mut().find(|lane| {
            lane.id.0 == transaction.sender()
                && lane.id.1
                    == transaction.transaction.eip8130_nonce_channel_key().unwrap_or_default()
        }) {
            lane.invalidated = true;
        }
    }

    fn no_updates(&mut self) {}

    fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_primitives::Bytes;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Signed, TxEip8130,
    };
    use reth_transaction_pool::{PoolTransaction, PriceBumpConfig, TransactionOrigin};

    use super::*;
    use crate::BasePooledTransaction;

    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn signed_channel_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            expiry: 0,
            max_priority_fee_per_gas: 0,
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
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        }
    }

    #[test]
    fn channelized_transactions_with_same_sequence_can_coexist() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000));
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(2), 0, 1_000));

        pool.insert_validated(first).unwrap();
        pool.insert_validated(second).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!(pending, 2);
        assert_eq!(queued, 0);
        assert_eq!(pool.all_transactions().len(), 2);
    }

    #[test]
    fn same_channel_sequence_replacement_is_lane_local() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let original = valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_000));
        let replacement =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_250));
        let original_hash = *original.hash();
        let replacement_hash = *replacement.hash();

        pool.insert_validated(original).unwrap();
        let outcome = pool.insert_validated(replacement).unwrap();

        assert_eq!(
            outcome.replaced.as_ref().map(|transaction| *transaction.hash()),
            Some(original_hash)
        );
        assert!(pool.get(&original_hash).is_none());
        assert!(pool.get(&replacement_hash).is_some());
        assert_eq!(pool.all_transactions().len(), 1);
    }

    #[test]
    fn pruning_mined_head_promotes_next_sequence_in_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let head = valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 0, 1_000));
        let head_hash = *head.hash();
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 1, 900));
        let queued_hash = *queued.hash();

        pool.insert_validated(head).unwrap();
        pool.insert_validated(queued).unwrap();

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (1, 1));

        pool.prune_mined(&[head_hash]);

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (1, 0));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![queued_hash]
        );
    }
}
