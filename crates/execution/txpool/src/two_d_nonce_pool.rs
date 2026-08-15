//! Minimal 2D nonce sidecar storage and iteration for channelized EIP-8130 transactions.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use alloy_primitives::{Address, TxHash, U256};
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolResult, PriceBumpConfig, Priority,
    TransactionOrdering, ValidPoolTransaction,
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

impl<T: BasePooledTx> NonceLane<T> {
    fn live_transactions(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions.range(self.next_nonce..).map(|(_, transaction)| transaction)
    }

    fn consecutive_pending_transactions(
        &self,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.live_transactions()
            .enumerate()
            .take_while(|(offset, transaction)| {
                self.next_nonce
                    .checked_add(*offset as u64)
                    .is_some_and(|expected| transaction.nonce() == expected)
            })
            .map(|(_, transaction)| transaction)
    }

    fn consecutive_pending_len(&self) -> usize {
        self.consecutive_pending_transactions().count()
    }

    fn queued_transactions(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.live_transactions().skip(self.consecutive_pending_len())
    }
}

/// Outcome returned after inserting into the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct InsertOutcome<T: BasePooledTx> {
    pub outcome: AddedTransactionOutcome,
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
}

/// Outcome returned after pruning mined transactions from the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct PruneMinedOutcome<T: BasePooledTx> {
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
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
            let pending_in_lane = lane.consecutive_pending_len();
            let live_in_lane = lane.live_transactions().count();
            pending += pending_in_lane;
            queued += live_in_lane.saturating_sub(pending_in_lane);
        }
        (pending, queued)
    }

    /// Returns all pending transactions.
    pub(crate) fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            for transaction in lane.consecutive_pending_transactions() {
                transactions.push(Arc::clone(transaction));
            }
        }
        transactions
    }

    /// Returns all queued transactions.
    pub(crate) fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            for transaction in lane.queued_transactions() {
                transactions.push(Arc::clone(transaction));
            }
        }
        transactions
    }

    /// Returns all transactions in the sidecar.
    pub(crate) fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            transactions.extend(lane.live_transactions().cloned());
        }
        transactions
    }

    /// Returns all transaction hashes in the sidecar.
    pub(crate) fn all_hashes(&self) -> Vec<TxHash> {
        let mut hashes = Vec::new();
        for lane in self.lanes.values() {
            hashes.extend(lane.live_transactions().map(|transaction| *transaction.hash()));
        }
        hashes
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
        let mut transactions = Vec::new();
        for ((lane_sender, _), lane) in &self.lanes {
            if *lane_sender == sender {
                transactions.extend(lane.live_transactions().cloned());
            }
        }
        transactions
    }

    /// Returns pending transactions for the given sender.
    pub(crate) fn pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.lanes
            .iter()
            .filter(|((lane_sender, _), _)| *lane_sender == sender)
            .flat_map(|(_, lane)| lane.consecutive_pending_transactions())
            .cloned()
            .collect()
    }

    /// Returns queued transactions for the given sender.
    pub(crate) fn queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.lanes
            .iter()
            .filter(|((lane_sender, _), _)| *lane_sender == sender)
            .flat_map(|(_, lane)| lane.queued_transactions())
            .cloned()
            .collect()
    }

    /// Returns all senders present in the sidecar.
    pub(crate) fn unique_senders(&self) -> HashSet<Address> {
        self.lanes.keys().map(|(sender, _)| *sender).collect()
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
        state_nonce: u64,
    ) -> PoolResult<InsertOutcome<T>> {
        let hash = *transaction.hash();
        if self.contains(&hash) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }

        let sender = transaction.sender();
        let nonce_key = transaction.transaction.eip8130_nonce_channel_key().ok_or_else(|| {
            PoolError::other(hash, "2D nonce pool only accepts channelized EIP-8130 transactions")
        })?;

        let lane_id = (sender, nonce_key);
        let sender_id = self.senders.sender_id_or_create(sender);
        let nonce = transaction.nonce();
        transaction.transaction_id = TransactionId::new(sender_id, nonce);
        let transaction = Arc::new(transaction);
        let lane = self.lanes.entry(lane_id).or_insert_with(|| NonceLane {
            next_nonce: state_nonce,
            transactions: BTreeMap::new(),
        });
        // Keep the lane anchored to the state view used by validation. This may
        // move backward after a reorg lowers the on-chain channel nonce, allowing
        // now-valid transactions to be accepted instead of treating them as
        // already executed under the pre-reorg lane cursor.
        if state_nonce != lane.next_nonce {
            lane.next_nonce = state_nonce;
        }
        let pending_len_before = lane.consecutive_pending_len();

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

        let pending_len_after = lane.consecutive_pending_len();
        let state = if lane
            .next_nonce
            .checked_add(pending_len_after as u64)
            .is_none_or(|boundary| nonce < boundary)
        {
            AddedTransactionState::Pending
        } else {
            AddedTransactionState::Queued(QueuedReason::NonceGap)
        };

        let promoted = if matches!(state, AddedTransactionState::Pending) {
            lane.consecutive_pending_transactions()
                .skip(pending_len_before)
                .filter(|candidate| *candidate.hash() != hash)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };

        Ok(InsertOutcome { outcome: AddedTransactionOutcome { hash, state }, replaced, promoted })
    }

    /// Removes the exact transactions by hash without advancing lane state.
    pub(crate) fn remove_transactions(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            if let Some(transaction) = self.remove_hash(*hash, false) {
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
    pub(crate) fn prune_mined(&mut self, hashes: &[TxHash]) -> PruneMinedOutcome<T> {
        let mut removed = Vec::new();
        let mut ordered_hashes: Vec<_> = hashes
            .iter()
            .filter_map(|hash| {
                self.index.get(hash).map(|(lane_id, nonce)| (lane_id.0, lane_id.1, *nonce, *hash))
            })
            .collect();
        ordered_hashes.sort_unstable();

        for (_, _, _, hash) in ordered_hashes {
            if let Some(transaction) = self.remove_hash(hash, true) {
                removed.push(transaction);
            }
        }

        PruneMinedOutcome { removed }
    }

    /// Removes sidecar transactions that can no longer afford the updated account balance.
    pub(crate) fn remove_unaffordable(
        &mut self,
        accounts: &[ChangedAccount],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut hashes = Vec::new();
        for account in accounts {
            hashes.extend(
                self.hashes
                    .values()
                    .filter(|transaction| {
                        transaction.sender() == account.address
                            && transaction.transaction.cost() > &account.balance
                    })
                    .map(|transaction| *transaction.hash()),
            );
        }
        hashes.sort_unstable();
        hashes.dedup();
        self.remove_transactions(&hashes)
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
    pub(crate) fn best_transactions<O>(
        &self,
        ordering: O,
        base_fee: u64,
    ) -> BestTwoDTransactions<T, O>
    where
        O: TransactionOrdering<Transaction = T>,
    {
        BestTwoDTransactions::new(&self.lanes, ordering, base_fee)
    }

    fn remove_hash(
        &mut self,
        hash: TxHash,
        advance_lane: bool,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let (lane_id, nonce) = *self.index.get(&hash)?;
        let transaction = {
            let lane = self.lanes.get_mut(&lane_id)?;
            let transaction = lane.transactions.remove(&nonce)?;
            if advance_lane
                && nonce == lane.next_nonce
                && let Some(next_nonce) = lane.next_nonce.checked_add(1)
            {
                lane.next_nonce = next_nonce;
            }
            transaction
        };

        if self.lanes.get(&lane_id).is_some_and(|lane| lane.transactions.is_empty()) {
            self.lanes.remove(&lane_id);
        }
        self.index.remove(&hash);
        self.hashes.remove(&hash);
        Some(transaction)
    }
}

/// Snapshot iterator over the current best transactions of the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct BestTwoDTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    lanes: Vec<LaneIterator<T>>,
    ordering: O,
    base_fee: u64,
}

#[derive(Debug)]
struct LaneIterator<T: BasePooledTx> {
    id: LaneId,
    transactions: Vec<Arc<ValidPoolTransaction<T>>>,
    index: usize,
    invalidated: bool,
}

impl<T: BasePooledTx, O> BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn new(lanes: &HashMap<LaneId, NonceLane<T>>, ordering: O, base_fee: u64) -> Self {
        let lanes = lanes
            .iter()
            .filter_map(|(id, lane)| {
                let mut next_nonce = lane.next_nonce;
                let mut transactions = Vec::new();
                while let Some(transaction) = lane.transactions.get(&next_nonce) {
                    transactions.push(Arc::clone(transaction));
                    let Some(incremented_nonce) = next_nonce.checked_add(1) else {
                        break;
                    };
                    next_nonce = incremented_nonce;
                }
                (!transactions.is_empty()).then(|| LaneIterator {
                    id: *id,
                    transactions,
                    index: 0,
                    invalidated: false,
                })
            })
            .collect();
        Self { lanes, ordering, base_fee }
    }

    fn priority_key(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) -> (Priority<O::PriorityValue>, Reverse<std::time::Instant>, TxHash) {
        (
            self.ordering.priority(&transaction.transaction, self.base_fee),
            Reverse(transaction.timestamp),
            *transaction.hash(),
        )
    }
}

impl<T: BasePooledTx, O> Iterator for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        let best_index = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| {
                if lane.invalidated || lane.index >= lane.transactions.len() {
                    None
                } else {
                    Some((index, self.priority_key(&lane.transactions[lane.index])))
                }
            })
            .max_by_key(|(_, priority)| priority.clone())
            .map(|(index, _)| index)?;

        let lane = &mut self.lanes[best_index];
        let transaction = Arc::clone(&lane.transactions[lane.index]);
        lane.index += 1;
        Some(transaction)
    }
}

impl<T: BasePooledTx, O> BestTransactions for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
        let Some(nonce_key) = transaction.transaction.eip8130_nonce_channel_key() else {
            return;
        };
        if let Some(lane) = self
            .lanes
            .iter_mut()
            .find(|lane| lane.id.0 == transaction.sender() && lane.id.1 == nonce_key)
        {
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
    use reth_execution_types::ChangedAccount;
    use reth_transaction_pool::{PoolTransaction, PriceBumpConfig, TransactionOrigin};

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction};

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
        signed_channel_tx_with_tip(signer, nonce_key, nonce_sequence, 0, max_fee_per_gas)
    }

    fn signed_channel_tx_with_tip(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
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
            metadata: Bytes::new(),
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
        valid_pool_transaction_at(transaction, Instant::now())
    }

    fn valid_pool_transaction_at(
        transaction: BasePooledTransaction,
        timestamp: Instant,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp,
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

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(second, 0).unwrap();

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

        pool.insert_validated(original, 0).unwrap();
        let outcome = pool.insert_validated(replacement, 0).unwrap();

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

        pool.insert_validated(head, 0).unwrap();
        pool.insert_validated(queued, 0).unwrap();

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (2, 0));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![head_hash, queued_hash]
        );

        let outcome = pool.prune_mined(&[head_hash]);
        assert_eq!(
            outcome.removed.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![head_hash]
        );

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (1, 0));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![queued_hash]
        );
    }

    #[test]
    fn remove_unaffordable_prunes_sidecar_transactions_for_changed_account() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let affordable = valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 0, 1));
        let affordable_hash = *affordable.hash();
        let unaffordable =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(4), 0, 1_000_000_000_000));
        let unaffordable_hash = *unaffordable.hash();

        pool.insert_validated(affordable, 0).unwrap();
        pool.insert_validated(unaffordable, 0).unwrap();

        let removed = pool.remove_unaffordable(&[ChangedAccount {
            address: signer.address(),
            nonce: 0,
            balance: U256::from(100_000u64),
        }]);

        assert_eq!(
            removed.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![unaffordable_hash]
        );
        assert!(pool.get(&unaffordable_hash).is_none());
        assert!(pool.get(&affordable_hash).is_some());
    }

    #[test]
    fn contiguous_lane_counts_full_run_as_pending() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 0, 1_000));
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 1, 900));
        let third = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 2, 800));
        let gap = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 4, 700));

        let first_hash = *first.hash();
        let second_hash = *second.hash();
        let third_hash = *third.hash();
        let gap_hash = *gap.hash();

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(second, 0).unwrap();
        pool.insert_validated(third, 0).unwrap();
        pool.insert_validated(gap, 0).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued), (3, 1));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![first_hash, second_hash, third_hash]
        );
        assert_eq!(
            pool.queued_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![gap_hash]
        );
    }

    #[test]
    fn queued_transactions_ignore_stale_nonces_below_lane_head() {
        let signer = signer();
        let stale =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 3, 1_000)));
        let first_pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 5, 900)));
        let second_pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 6, 800)));
        let queued =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 10, 700)));

        let lane = NonceLane {
            next_nonce: 5,
            transactions: BTreeMap::from([
                (3, Arc::clone(&stale)),
                (5, Arc::clone(&first_pending)),
                (6, Arc::clone(&second_pending)),
                (10, Arc::clone(&queued)),
            ]),
        };

        assert_eq!(
            lane.queued_transactions().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![*queued.hash()]
        );
    }

    #[test]
    fn consecutive_pending_handles_u64_max_nonce_without_overflow() {
        let signer = signer();
        let transaction = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(16),
            u64::MAX,
            1_000,
        )));

        let lane = NonceLane {
            next_nonce: u64::MAX,
            transactions: BTreeMap::from([(u64::MAX, Arc::clone(&transaction))]),
        };

        assert_eq!(
            lane.consecutive_pending_transactions()
                .map(|transaction| *transaction.hash())
                .collect::<Vec<_>>(),
            vec![*transaction.hash()]
        );
        assert!(lane.queued_transactions().next().is_none());
    }

    #[test]
    fn all_transactions_and_hashes_skip_stale_entries_below_lane_head() {
        let signer = signer();
        let stale =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(23), 3, 1_000)));
        let pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(23), 5, 900)));
        let queued =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(23), 7, 800)));
        let lane_id = (signer.address(), U256::from(23));

        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        pool.hashes.insert(*stale.hash(), Arc::clone(&stale));
        pool.hashes.insert(*pending.hash(), Arc::clone(&pending));
        pool.hashes.insert(*queued.hash(), Arc::clone(&queued));
        pool.index.insert(*stale.hash(), (lane_id, 3));
        pool.index.insert(*pending.hash(), (lane_id, 5));
        pool.index.insert(*queued.hash(), (lane_id, 7));
        pool.lanes.insert(
            lane_id,
            NonceLane {
                next_nonce: 5,
                transactions: BTreeMap::from([
                    (3, Arc::clone(&stale)),
                    (5, Arc::clone(&pending)),
                    (7, Arc::clone(&queued)),
                ]),
            },
        );

        assert_eq!(
            pool.all_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![*pending.hash(), *queued.hash()]
        );
        assert_eq!(pool.all_hashes(), vec![*pending.hash(), *queued.hash()]);
        assert_eq!(
            pool.transactions_by_sender(signer.address())
                .into_iter()
                .map(|tx| *tx.hash())
                .collect::<Vec<_>>(),
            vec![*pending.hash(), *queued.hash()]
        );
        assert_eq!(pool.pending_and_queued_txn_count(), (1, 1));
        assert_eq!(pool.unique_senders(), HashSet::from([signer.address()]));
    }

    #[test]
    fn best_transactions_snapshot_handles_u64_max_nonce_without_wrapping() {
        let signer = signer();
        let transaction = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(17),
            u64::MAX,
            1_000,
        )));
        let lane_id = (signer.address(), U256::from(17));
        let lanes = HashMap::from([(
            lane_id,
            NonceLane {
                next_nonce: u64::MAX,
                transactions: BTreeMap::from([(u64::MAX, Arc::clone(&transaction))]),
            },
        )]);

        let mut best = BestTwoDTransactions::new(&lanes, BaseOrdering::coinbase_tip(), 0);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(*transaction.hash()));
        assert!(best.next().is_none());
    }

    #[test]
    fn insert_validated_classifies_u64_max_head_as_pending_without_overflow() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let transaction =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(18), u64::MAX, 1_000));

        let lane_id = (signer.address(), U256::from(18));
        pool.lanes
            .insert(lane_id, NonceLane { next_nonce: u64::MAX, transactions: BTreeMap::new() });

        let outcome = pool.insert_validated(transaction, u64::MAX).unwrap();

        assert!(matches!(outcome.outcome.state, AddedTransactionState::Pending));
    }

    #[test]
    fn prune_mined_does_not_wrap_lane_head_after_u64_max() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let head = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(19),
            u64::MAX,
            1_000,
        )));
        let head_hash = *head.hash();
        let stale =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(19), 7, 900)));
        let lane_id = (signer.address(), U256::from(19));

        pool.hashes.insert(head_hash, Arc::clone(&head));
        pool.index.insert(head_hash, (lane_id, u64::MAX));
        pool.lanes.insert(
            lane_id,
            NonceLane {
                next_nonce: u64::MAX,
                transactions: BTreeMap::from([(7, Arc::clone(&stale)), (u64::MAX, head)]),
            },
        );

        let _ = pool.prune_mined(&[head_hash]);

        assert_eq!(pool.lanes.get(&lane_id).map(|lane| lane.next_nonce), Some(u64::MAX));
    }

    #[test]
    fn gap_fill_reports_newly_promoted_transactions() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 0, 1_000));
        let gap = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 2, 800));
        let middle = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 1, 900));
        let gap_hash = *gap.hash();

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(gap, 0).unwrap();

        let outcome = pool.insert_validated(middle, 0).unwrap();

        assert_eq!(
            outcome.promoted.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![gap_hash]
        );
    }

    #[test]
    fn pruning_mined_sorts_hashes_within_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 0, 1_000));
        let first_hash = *first.hash();
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 1, 900));
        let second_hash = *second.hash();
        let third = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 2, 800));
        let third_hash = *third.hash();
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 4, 700));

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(second, 0).unwrap();
        pool.insert_validated(third, 0).unwrap();
        pool.insert_validated(queued, 0).unwrap();

        pool.prune_mined(&[third_hash, first_hash, second_hash]);

        let replacement =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 2, 850));
        let error = pool.insert_validated(replacement, 3).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::InvalidTransaction(_)));
    }

    #[test]
    fn inserting_non_channelized_transaction_returns_error() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let non_channelized =
            valid_pool_transaction(signed_channel_tx(&signer, U256::ZERO, 0, 1_000));

        let error = pool.insert_validated(non_channelized, 0).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::Other(_)));
    }

    #[test]
    fn mark_invalid_only_invalidates_matching_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first_lane_head =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(21), 0, 1_000));
        let first_lane_head_hash = *first_lane_head.hash();
        let first_lane_next =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(21), 1, 900));
        let second_lane_head =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(22), 0, 950));
        let second_lane_head_hash = *second_lane_head.hash();

        pool.insert_validated(first_lane_head, 0).unwrap();
        pool.insert_validated(first_lane_next, 0).unwrap();
        pool.insert_validated(second_lane_head, 0).unwrap();

        let lane_to_invalidate = pool.get(&first_lane_head_hash).unwrap();
        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 0);
        best.mark_invalid(
            &lane_to_invalidate,
            InvalidPoolTransactionError::Consensus(InvalidTransactionError::TxTypeNotSupported),
        );

        let yielded_hashes: Vec<_> = best.map(|transaction| *transaction.hash()).collect();
        assert_eq!(yielded_hashes.len(), 1);
        assert_eq!(yielded_hashes[0], second_lane_head_hash);
    }

    #[test]
    fn best_transactions_uses_effective_tip_across_sidecar_lanes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let low_tip_high_cap =
            valid_pool_transaction(signed_channel_tx_with_tip(&signer, U256::from(31), 0, 1, 100));
        let high_tip_lower_cap =
            valid_pool_transaction(signed_channel_tx_with_tip(&signer, U256::from(32), 0, 50, 50));
        let high_tip_hash = *high_tip_lower_cap.hash();

        pool.insert_validated(low_tip_high_cap, 0).unwrap();
        pool.insert_validated(high_tip_lower_cap, 0).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 10);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(high_tip_hash));
    }

    #[test]
    fn equal_priority_prefers_earlier_submission_timestamp_across_sidecar_lanes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let now = Instant::now();

        let older = valid_pool_transaction_at(
            signed_channel_tx_with_tip(&signer, U256::from(41), 0, 10, 100),
            now,
        );
        let older_hash = *older.hash();
        let newer = valid_pool_transaction_at(
            signed_channel_tx_with_tip(&signer, U256::from(42), 0, 10, 100),
            now + std::time::Duration::from_secs(1),
        );

        pool.insert_validated(older, 0).unwrap();
        pool.insert_validated(newer, 0).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 10);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(older_hash));
    }
}
