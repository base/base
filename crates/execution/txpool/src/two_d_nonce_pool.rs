//! Sidecar storage and iteration for channelized and nonce-free EIP-8130 transactions.

use std::{
    collections::{BTreeMap, BinaryHeap, HashSet},
    sync::Arc,
};

use alloy_primitives::{
    Address, TxHash, U256,
    map::{B256Map, HashMap},
};
use base_common_consensus::Eip8130Constants;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
#[cfg(test)]
use reth_transaction_pool::PriceBumpConfig;
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolConfig, PoolResult, SubPool,
    TransactionOrdering, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind},
    identifier::{SenderIdentifiers, TransactionId},
    pool::{AddedTransactionState, QueuedReason},
};

use crate::{BasePooledTx, BaseTransactionIdentity, BaseTransactionLane, BestTransactionPriority};

type LaneId = (Address, U256);

#[derive(Debug)]
struct NonceLane<T: BasePooledTx> {
    next_nonce: u64,
    transactions: BTreeMap<u64, Arc<ValidPoolTransaction<T>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidecarSubPool {
    Pending,
    BaseFee,
    Queued,
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

    fn classified_transactions(
        &self,
        base_fee: u64,
    ) -> Vec<(SidecarSubPool, &Arc<ValidPoolTransaction<T>>)> {
        let mut expected_nonce = self.next_nonce;
        let mut blocked = false;
        self.live_transactions()
            .map(|transaction| {
                let subpool = if blocked || transaction.nonce() != expected_nonce {
                    blocked = true;
                    SidecarSubPool::Queued
                } else if transaction.transaction.max_fee_per_gas() < u128::from(base_fee) {
                    blocked = true;
                    SidecarSubPool::BaseFee
                } else {
                    SidecarSubPool::Pending
                };
                expected_nonce = expected_nonce.saturating_add(1);
                (subpool, transaction)
            })
            .collect()
    }

    fn transactions_in(
        &self,
        subpool: SidecarSubPool,
        base_fee: u64,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.classified_transactions(base_fee).into_iter().filter_map(
            move |(candidate, transaction)| (candidate == subpool).then_some(transaction),
        )
    }

    #[cfg(test)]
    fn consecutive_pending_transactions(
        &self,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions_in(SidecarSubPool::Pending, 0)
    }

    #[cfg(test)]
    fn queued_transactions(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions_in(SidecarSubPool::Queued, 0)
    }
}

/// Outcome returned after inserting into the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct InsertOutcome<T: BasePooledTx> {
    pub outcome: AddedTransactionOutcome,
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
}

/// Sidecar transactions whose pending status changed after a base-fee update.
#[derive(Debug)]
pub(crate) struct FeeUpdateOutcome<T: BasePooledTx> {
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    pub demoted: Vec<(Arc<ValidPoolTransaction<T>>, SubPool)>,
}

/// Outcome returned after pruning mined transactions from the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct PruneMinedOutcome<T: BasePooledTx> {
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
}

/// EIP-8130 sidecar for finite non-zero nonce channels and nonce-free transactions.
///
/// Finite channels are kept in ordered `(sender, nonce_key)` lanes. Nonce-free
/// transactions have no sequencing relationship, so they are stored separately
/// by replay id and compete independently in the best-transactions iterator.
#[derive(Debug)]
pub(crate) struct TwoDNoncePool<T: BasePooledTx> {
    lanes: HashMap<LaneId, NonceLane<T>>,
    nonce_free: B256Map<Arc<ValidPoolTransaction<T>>>,
    hashes: B256Map<Arc<ValidPoolTransaction<T>>>,
    senders: SenderIdentifiers,
    config: PoolConfig,
    base_fee: u64,
}

impl<T: BasePooledTx> TwoDNoncePool<T> {
    #[cfg(test)]
    pub(crate) fn new(price_bumps: PriceBumpConfig) -> Self {
        Self::new_with_config(
            PoolConfig { price_bumps, max_account_slots: usize::MAX, ..PoolConfig::default() },
            0,
        )
    }

    /// Creates a new 2D nonce sidecar pool using the protocol pool configuration.
    pub(crate) fn new_with_config(config: PoolConfig, base_fee: u64) -> Self {
        Self {
            lanes: HashMap::default(),
            nonce_free: B256Map::default(),
            hashes: B256Map::default(),
            senders: SenderIdentifiers::default(),
            config,
            base_fee,
        }
    }

    /// Returns true if the sidecar already contains the hash.
    pub(crate) fn contains(&self, hash: &TxHash) -> bool {
        self.hashes.contains_key(hash)
    }

    /// Returns the number of pending and queued transactions.
    pub(crate) fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let (pending, basefee, queued) = self.subpool_counts();
        (pending, basefee + queued)
    }

    /// Returns pending, base-fee, and queued transaction counts and sizes.
    pub(crate) fn subpool_size(&self) -> [(usize, usize); 3] {
        let mut sizes = [(0, 0); 3];
        for (subpool, transaction) in self.classified_transactions() {
            let index = match subpool {
                SidecarSubPool::Pending => 0,
                SidecarSubPool::BaseFee => 1,
                SidecarSubPool::Queued => 2,
            };
            sizes[index].0 += 1;
            sizes[index].1 += transaction.encoded_length();
        }
        sizes
    }

    /// Returns all pending transactions.
    pub(crate) fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            for transaction in lane.transactions_in(SidecarSubPool::Pending, self.base_fee) {
                transactions.push(Arc::clone(transaction));
            }
        }
        transactions.extend(
            self.nonce_free
                .values()
                .filter(|transaction| {
                    transaction.transaction.max_fee_per_gas() >= u128::from(self.base_fee)
                })
                .cloned(),
        );
        transactions
    }

    /// Returns transactions parked because their fee cap is below the current base fee.
    pub(crate) fn basefee_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.transactions_in(SidecarSubPool::BaseFee)
    }

    /// Returns all queued transactions.
    pub(crate) fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            for transaction in lane.transactions_in(SidecarSubPool::Queued, self.base_fee) {
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
        transactions.extend(self.nonce_free.values().cloned());
        transactions
    }

    /// Returns all transaction hashes in the sidecar.
    pub(crate) fn all_hashes(&self) -> Vec<TxHash> {
        let mut hashes = Vec::new();
        for lane in self.lanes.values() {
            hashes.extend(lane.live_transactions().map(|transaction| *transaction.hash()));
        }
        hashes.extend(self.nonce_free.values().map(|transaction| *transaction.hash()));
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
        transactions.extend(
            self.nonce_free.values().filter(|transaction| transaction.sender() == sender).cloned(),
        );
        transactions
    }

    /// Returns pending transactions for the given sender.
    pub(crate) fn pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions: Vec<_> = self
            .lanes
            .iter()
            .filter(|((lane_sender, _), _)| *lane_sender == sender)
            .flat_map(|(_, lane)| lane.transactions_in(SidecarSubPool::Pending, self.base_fee))
            .cloned()
            .collect();
        transactions.extend(
            self.nonce_free
                .values()
                .filter(|transaction| {
                    transaction.sender() == sender
                        && transaction.transaction.max_fee_per_gas() >= u128::from(self.base_fee)
                })
                .cloned(),
        );
        transactions
    }

    /// Returns queued transactions for the given sender.
    pub(crate) fn queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.lanes
            .iter()
            .filter(|((lane_sender, _), _)| *lane_sender == sender)
            .flat_map(|(_, lane)| {
                lane.classified_transactions(self.base_fee).into_iter().filter_map(
                    |(subpool, transaction)| {
                        (subpool != SidecarSubPool::Pending).then_some(transaction)
                    },
                )
            })
            .cloned()
            .collect()
    }

    /// Returns all senders present in the sidecar.
    pub(crate) fn unique_senders(&self) -> HashSet<Address> {
        self.lanes
            .keys()
            .map(|(sender, _)| *sender)
            .chain(self.nonce_free.values().map(|transaction| transaction.sender()))
            .collect()
    }

    /// Returns or creates the sender id for the given address.
    pub(crate) fn sender_id_or_create(
        &mut self,
        address: Address,
    ) -> reth_transaction_pool::identifier::SenderId {
        self.senders.sender_id_or_create(address)
    }

    /// Inserts a validated sidecar EIP-8130 transaction.
    ///
    /// Nonce-free transactions replace only another transaction with the same
    /// replay id. Finite-channel transactions retain their lane-local sequence
    /// replacement semantics.
    pub(crate) fn insert_validated(
        &mut self,
        mut transaction: ValidPoolTransaction<T>,
        state_nonce: u64,
    ) -> PoolResult<InsertOutcome<T>> {
        let hash = *transaction.hash();
        if self.contains(&hash) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }

        if let BaseTransactionIdentity::Replay { replay_id } = transaction.transaction.identity() {
            let sender_id = self.senders.sender_id_or_create(transaction.sender());
            transaction.transaction_id = TransactionId::new(sender_id, transaction.nonce());
            let transaction = Arc::new(transaction);
            let replaced = if let Some(existing) = self.nonce_free.get(&replay_id) {
                if existing.is_underpriced(&transaction, &self.config.price_bumps) {
                    return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
                }
                Some(Arc::clone(existing))
            } else {
                None
            };
            self.ensure_sender_capacity(&transaction, replaced.is_some())?;
            if let Some(existing) = &replaced {
                self.hashes.remove(existing.hash());
            }
            self.nonce_free.insert(replay_id, Arc::clone(&transaction));
            self.hashes.insert(hash, Arc::clone(&transaction));
            return Ok(InsertOutcome {
                outcome: AddedTransactionOutcome {
                    hash,
                    state: self.added_state(&transaction, SidecarSubPool::Pending),
                },
                replaced,
                promoted: Vec::new(),
            });
        }

        let BaseTransactionIdentity::Nonce {
            lane: BaseTransactionLane::Channel { sender, nonce_key },
            nonce,
        } = transaction.transaction.identity()
        else {
            return Err(PoolError::other(
                hash,
                "2D nonce pool only accepts sidecar EIP-8130 transactions",
            ));
        };

        let lane_id = (sender, nonce_key);
        let sender_id = self.senders.sender_id_or_create(sender);
        transaction.transaction_id = TransactionId::new(sender_id, nonce);
        let transaction = Arc::new(transaction);
        let replaced = {
            let lane = self.lanes.entry(lane_id).or_insert_with(|| NonceLane {
                next_nonce: state_nonce,
                transactions: BTreeMap::new(),
            });
            // Keep the lane anchored to the state view used by validation. This may
            // move backward after a reorg lowers the on-chain channel nonce.
            lane.next_nonce = state_nonce;
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
            if let Some(existing) = lane.transactions.get(&nonce) {
                if existing.is_underpriced(&transaction, &self.config.price_bumps) {
                    return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
                }
                Some(Arc::clone(existing))
            } else {
                None
            }
        };
        self.ensure_sender_capacity(&transaction, replaced.is_some())?;

        let pending_before: HashSet<_> = self
            .lanes
            .get(&lane_id)
            .into_iter()
            .flat_map(|lane| lane.transactions_in(SidecarSubPool::Pending, self.base_fee))
            .map(|transaction| *transaction.hash())
            .collect();
        let lane = self.lanes.get_mut(&lane_id).expect("lane was initialized");

        lane.transactions.insert(nonce, Arc::clone(&transaction));
        self.hashes.insert(hash, Arc::clone(&transaction));

        if let Some(replaced) = &replaced {
            let replaced_hash = *replaced.hash();
            self.hashes.remove(&replaced_hash);
        }

        let subpool = lane
            .classified_transactions(self.base_fee)
            .into_iter()
            .find_map(|(subpool, candidate)| (*candidate.hash() == hash).then_some(subpool))
            .expect("inserted transaction is classified");
        let state = Self::state_for(subpool);
        let promoted = lane
            .transactions_in(SidecarSubPool::Pending, self.base_fee)
            .filter(|candidate| {
                *candidate.hash() != hash && !pending_before.contains(candidate.hash())
            })
            .cloned()
            .collect();

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
            if self.hashes.get(hash).is_some_and(|transaction| {
                matches!(transaction.transaction.identity(), BaseTransactionIdentity::Replay { .. })
            }) {
                if let Some(transaction) = self.remove_hash(*hash, false) {
                    removed.push(transaction);
                }
                continue;
            }
            let Some(transaction) = self.hashes.get(hash) else {
                continue;
            };
            let BaseTransactionIdentity::Nonce {
                lane: BaseTransactionLane::Channel { nonce_key, .. },
                ..
            } = transaction.transaction.identity()
            else {
                continue;
            };
            let lane_id = (transaction.sender(), nonce_key);
            let nonce = transaction.nonce();
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
        for hash in hashes {
            if self.hashes.get(hash).is_some_and(|transaction| {
                matches!(transaction.transaction.identity(), BaseTransactionIdentity::Replay { .. })
            }) && let Some(transaction) = self.remove_hash(*hash, false)
            {
                removed.push(transaction);
            }
        }
        let mut ordered_hashes: Vec<_> = hashes
            .iter()
            .filter_map(|hash| {
                let transaction = self.hashes.get(hash)?;
                let BaseTransactionIdentity::Nonce {
                    lane: BaseTransactionLane::Channel { sender, nonce_key },
                    nonce,
                } = transaction.transaction.identity()
                else {
                    return None;
                };
                Some((sender, nonce_key, nonce, *hash))
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

    /// Removes nonce-free transactions whose validity window has elapsed at
    /// `now` (Unix **milliseconds**, i.e. `block.timestamp * 1000`).
    ///
    /// A nonce-free transaction is invalid when `valid_before <= now`, matching
    /// its structural validation rule. Finite channels are unaffected; their
    /// optional window is handled by normal transaction validation until the
    /// state-keyed expiry index is introduced.
    pub(crate) fn remove_expired_nonce_free(
        &mut self,
        now: u64,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let expired: Vec<TxHash> = self
            .nonce_free
            .values()
            .filter_map(|transaction| {
                let signed = transaction.transaction.as_eip8130()?;
                (signed.tx().valid_before <= now).then_some(*transaction.hash())
            })
            .collect();
        self.remove_transactions(&expired)
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

    /// Applies a new base fee and reports transactions entering or leaving pending.
    pub(crate) fn set_base_fee(&mut self, base_fee: u64) -> FeeUpdateOutcome<T> {
        if self.base_fee == base_fee {
            return FeeUpdateOutcome { promoted: Vec::new(), demoted: Vec::new() };
        }
        let before: B256Map<_> = self
            .classified_transactions()
            .map(|(subpool, transaction)| (*transaction.hash(), subpool))
            .collect();
        self.base_fee = base_fee;
        let mut promoted = Vec::new();
        let mut demoted = Vec::new();
        for (subpool, transaction) in self.classified_transactions() {
            match (before.get(transaction.hash()), subpool) {
                (Some(SidecarSubPool::Pending), SidecarSubPool::BaseFee) => {
                    demoted.push((Arc::clone(transaction), SubPool::BaseFee));
                }
                (Some(SidecarSubPool::Pending), SidecarSubPool::Queued) => {
                    demoted.push((Arc::clone(transaction), SubPool::Queued));
                }
                (Some(previous), SidecarSubPool::Pending)
                    if *previous != SidecarSubPool::Pending =>
                {
                    promoted.push(Arc::clone(transaction));
                }
                _ => {}
            }
        }
        FeeUpdateOutcome { promoted, demoted }
    }

    /// Evicts non-local transactions until all configured sidecar subpool limits hold.
    pub(crate) fn enforce_limits(&mut self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut discarded = Vec::new();
        loop {
            let sizes = self.subpool_size();
            let exceeded = [
                (SidecarSubPool::Pending, self.config.pending_limit, sizes[0]),
                (SidecarSubPool::BaseFee, self.config.basefee_limit, sizes[1]),
                (SidecarSubPool::Queued, self.config.queued_limit, sizes[2]),
            ]
            .into_iter()
            .find_map(|(subpool, limit, (count, size))| {
                limit.is_exceeded(count, size).then_some(subpool)
            });
            let Some(exceeded) = exceeded else { break };
            let victim = self
                .transactions_in(exceeded)
                .into_iter()
                .filter(|transaction| {
                    !self
                        .config
                        .local_transactions_config
                        .is_local(transaction.origin, transaction.sender_ref())
                })
                .min_by_key(|transaction| {
                    (
                        transaction.transaction.max_fee_per_gas(),
                        transaction.transaction.priority_fee_or_price(),
                        *transaction.hash(),
                    )
                });
            let Some(victim) = victim else { break };
            discarded.extend(self.remove_transactions_and_descendants(&[*victim.hash()]));
        }
        discarded
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
        BestTwoDTransactions::new(&self.lanes, &self.nonce_free, ordering, base_fee)
    }

    fn ensure_sender_capacity(
        &self,
        transaction: &ValidPoolTransaction<T>,
        replacing: bool,
    ) -> PoolResult<()> {
        if replacing
            || self
                .config
                .local_transactions_config
                .is_local(transaction.origin, transaction.sender_ref())
        {
            return Ok(());
        }
        let sender = transaction.sender();
        if self.transactions_by_sender(sender).len() >= self.config.max_account_slots {
            return Err(PoolError::new(
                *transaction.hash(),
                PoolErrorKind::SpammerExceededCapacity(sender),
            ));
        }
        Ok(())
    }

    fn added_state(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
        otherwise: SidecarSubPool,
    ) -> AddedTransactionState {
        if transaction.transaction.max_fee_per_gas() < u128::from(self.base_fee) {
            Self::state_for(SidecarSubPool::BaseFee)
        } else {
            Self::state_for(otherwise)
        }
    }

    const fn state_for(subpool: SidecarSubPool) -> AddedTransactionState {
        match subpool {
            SidecarSubPool::Pending => AddedTransactionState::Pending,
            SidecarSubPool::BaseFee => {
                AddedTransactionState::Queued(QueuedReason::InsufficientBaseFee)
            }
            SidecarSubPool::Queued => AddedTransactionState::Queued(QueuedReason::NonceGap),
        }
    }

    fn subpool_counts(&self) -> (usize, usize, usize) {
        let sizes = self.subpool_size();
        (sizes[0].0, sizes[1].0, sizes[2].0)
    }

    fn classified_transactions(
        &self,
    ) -> impl Iterator<Item = (SidecarSubPool, &Arc<ValidPoolTransaction<T>>)> {
        self.lanes.values().flat_map(|lane| lane.classified_transactions(self.base_fee)).chain(
            self.nonce_free.values().map(|transaction| {
                let subpool =
                    if transaction.transaction.max_fee_per_gas() < u128::from(self.base_fee) {
                        SidecarSubPool::BaseFee
                    } else {
                        SidecarSubPool::Pending
                    };
                (subpool, transaction)
            }),
        )
    }

    fn transactions_in(&self, subpool: SidecarSubPool) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.classified_transactions()
            .filter(|(candidate, _)| *candidate == subpool)
            .map(|(_, transaction)| Arc::clone(transaction))
            .collect()
    }

    fn remove_hash(
        &mut self,
        hash: TxHash,
        advance_lane: bool,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        if let Some(transaction) = self.hashes.get(&hash)
            && let BaseTransactionIdentity::Replay { replay_id } =
                transaction.transaction.identity()
        {
            let transaction = self.nonce_free.remove(&replay_id)?;
            self.hashes.remove(&hash);
            return Some(transaction);
        }
        let transaction = self.hashes.get(&hash)?;
        let BaseTransactionIdentity::Nonce {
            lane: BaseTransactionLane::Channel { sender, nonce_key },
            nonce,
        } = transaction.transaction.identity()
        else {
            return None;
        };
        let lane_id = (sender, nonce_key);
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
        self.hashes.remove(&hash);
        Some(transaction)
    }
}

/// Snapshot iterator over the current best transactions of the EIP-8130 sidecar.
///
/// Each finite channel contributes its contiguous head and each nonce-free
/// transaction contributes an independent one-item candidate.
#[derive(Debug)]
pub(crate) struct BestTwoDTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    lanes: Vec<LaneIterator<T>>,
    candidates: BinaryHeap<(BestTransactionPriority<O::PriorityValue>, usize)>,
    lane_indexes: HashMap<LaneId, usize>,
    nonce_free_indexes: HashMap<TxHash, usize>,
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
    fn new(
        lanes: &HashMap<LaneId, NonceLane<T>>,
        nonce_free: &B256Map<Arc<ValidPoolTransaction<T>>>,
        ordering: O,
        base_fee: u64,
    ) -> Self {
        let mut lanes: Vec<_> = lanes
            .iter()
            .filter_map(|(id, lane)| {
                let mut next_nonce = lane.next_nonce;
                let mut transactions = Vec::new();
                while let Some(transaction) = lane.transactions.get(&next_nonce) {
                    if transaction.transaction.max_fee_per_gas() < u128::from(base_fee) {
                        break;
                    }
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
        let finite_lane_count = lanes.len();
        lanes.extend(
            nonce_free
                .values()
                .filter(|transaction| {
                    transaction.transaction.max_fee_per_gas() >= u128::from(base_fee)
                })
                .map(|transaction| LaneIterator {
                    id: (transaction.sender(), Eip8130Constants::NONCE_KEY_MAX),
                    transactions: vec![Arc::clone(transaction)],
                    index: 0,
                    invalidated: false,
                }),
        );
        let lane_indexes = lanes[..finite_lane_count]
            .iter()
            .enumerate()
            .map(|(index, lane)| (lane.id, index))
            .collect();
        let nonce_free_indexes = lanes[finite_lane_count..]
            .iter()
            .enumerate()
            .map(|(offset, lane)| (*lane.transactions[0].hash(), finite_lane_count + offset))
            .collect();
        let candidates = BinaryHeap::from(
            lanes
                .iter()
                .enumerate()
                .map(|(index, lane)| {
                    (
                        BestTransactionPriority::new(&ordering, &lane.transactions[0], base_fee),
                        index,
                    )
                })
                .collect::<Vec<_>>(),
        );
        Self { candidates, lanes, lane_indexes, nonce_free_indexes, ordering, base_fee }
    }

    fn priority_key(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) -> BestTransactionPriority<O::PriorityValue> {
        BestTransactionPriority::new(&self.ordering, transaction, self.base_fee)
    }

    fn push_lane_head(&mut self, index: usize) {
        let lane = &self.lanes[index];
        if lane.invalidated || lane.index >= lane.transactions.len() {
            return;
        }
        let priority = self.priority_key(&lane.transactions[lane.index]);
        self.candidates.push((priority, index));
    }
}

impl<T: BasePooledTx, O> Iterator for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (_, best_index) = self.candidates.pop()?;
            let lane = &mut self.lanes[best_index];
            if lane.invalidated {
                continue;
            }
            let transaction = Arc::clone(&lane.transactions[lane.index]);
            lane.index += 1;
            self.push_lane_head(best_index);
            return Some(transaction);
        }
    }
}

impl<T: BasePooledTx, O> BestTransactions for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
        let index = match transaction.transaction.identity() {
            BaseTransactionIdentity::Nonce {
                lane: BaseTransactionLane::Channel { sender, nonce_key },
                ..
            } => self.lane_indexes.get(&(sender, nonce_key)).copied(),
            BaseTransactionIdentity::Replay { .. } => {
                self.nonce_free_indexes.get(transaction.hash()).copied()
            }
            BaseTransactionIdentity::Nonce {
                lane: BaseTransactionLane::Protocol { .. }, ..
            } => None,
        };
        if let Some(index) = index {
            self.lanes[index].invalidated = true;
        }
    }

    fn no_updates(&mut self) {}

    fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Instant,
    };

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_primitives::Bytes;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Signed, TxEip8130,
    };
    use reth_transaction_pool::{
        PoolTransaction, PriceBumpConfig, Priority, SubPoolLimit, TransactionOrigin,
    };

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction};

    #[derive(Clone, Debug, Default)]
    struct CountingOrdering {
        priority_evaluations: Arc<AtomicUsize>,
    }

    impl TransactionOrdering for CountingOrdering {
        type PriorityValue = u128;
        type Transaction = BasePooledTransaction;

        fn priority(
            &self,
            transaction: &Self::Transaction,
            base_fee: u64,
        ) -> Priority<Self::PriorityValue> {
            self.priority_evaluations.fetch_add(1, Ordering::Relaxed);
            transaction.effective_tip_per_gas(base_fee).into()
        }
    }

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
        signed_tx(signer, nonce_key, nonce_sequence, 0, max_priority_fee_per_gas, max_fee_per_gas)
    }

    fn signed_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        valid_before: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before,
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

    fn signed_nonce_free_tx(
        signer: &PrivateKeySigner,
        valid_before: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_tx(
            signer,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            valid_before,
            max_priority_fee_per_gas,
            max_fee_per_gas,
        )
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

    fn pool_with_config(config: PoolConfig, base_fee: u64) -> TwoDNoncePool<BasePooledTransaction> {
        TwoDNoncePool::new_with_config(config, base_fee)
    }

    fn best_transaction_priority_evaluations(lane_count: usize) -> usize {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        for nonce_key in 1..=lane_count {
            let transaction = valid_pool_transaction(signed_channel_tx(
                &signer,
                U256::from(nonce_key),
                0,
                nonce_key as u128,
            ));
            pool.insert_validated(transaction, 0).unwrap();
        }

        let ordering = CountingOrdering::default();
        let priority_evaluations = Arc::clone(&ordering.priority_evaluations);
        let yielded = pool.best_transactions(ordering, 0).count();

        assert_eq!(yielded, lane_count);
        priority_evaluations.load(Ordering::Relaxed)
    }

    fn run_best_transactions_wall_clock(lane_count: usize) {
        let signer = signer();
        let transaction =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000)));

        let setup_started = Instant::now();
        let lanes = (1..=lane_count)
            .map(|nonce_key| {
                (
                    (signer.address(), U256::from(nonce_key)),
                    NonceLane {
                        next_nonce: 0,
                        transactions: BTreeMap::from([(0, Arc::clone(&transaction))]),
                    },
                )
            })
            .collect();
        let setup_elapsed = setup_started.elapsed();

        let snapshot_started = Instant::now();
        let best =
            BestTwoDTransactions::new(&lanes, &B256Map::default(), BaseOrdering::coinbase_tip(), 0);
        let snapshot_elapsed = snapshot_started.elapsed();

        let drain_started = Instant::now();
        let yielded = best.count();
        let drain_elapsed = drain_started.elapsed();

        eprintln!(
            "{lane_count:>6} lanes: setup={setup_elapsed:?}, snapshot={snapshot_elapsed:?}, drain={drain_elapsed:?}"
        );
        assert_eq!(yielded, lane_count);
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
    fn best_transactions_evaluates_each_lane_head_once() {
        const SMALL_LANE_COUNT: usize = 16;
        const LARGE_LANE_COUNT: usize = SMALL_LANE_COUNT * 2;

        let small_evaluations = best_transaction_priority_evaluations(SMALL_LANE_COUNT);
        let large_evaluations = best_transaction_priority_evaluations(LARGE_LANE_COUNT);

        assert_eq!(small_evaluations, SMALL_LANE_COUNT);
        assert_eq!(large_evaluations, LARGE_LANE_COUNT);
    }

    #[test]
    #[ignore = "wall-clock diagnostic; run explicitly in release mode with --ignored --nocapture"]
    fn best_transactions_wall_clock_1k_lanes() {
        run_best_transactions_wall_clock(1_000);
    }

    #[test]
    #[ignore = "wall-clock diagnostic; run explicitly in release mode with --ignored --nocapture"]
    fn best_transactions_wall_clock_10k_lanes() {
        run_best_transactions_wall_clock(10_000);
    }

    #[test]
    #[ignore = "wall-clock diagnostic; run explicitly in release mode with --ignored --nocapture"]
    fn best_transactions_wall_clock_100k_lanes() {
        run_best_transactions_wall_clock(100_000);
    }

    #[test]
    fn nonce_free_transactions_coexist_and_replace_atomically_by_replay_id() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let first = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 1_000));
        let distinct = valid_pool_transaction(signed_nonce_free_tx(&signer, 2, 1, 1_000));
        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(distinct, 0).unwrap();
        assert_eq!(pool.pending_and_queued_txn_count(), (2, 0));

        let underpriced = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 1_050));
        assert!(matches!(
            pool.insert_validated(underpriced, 0).unwrap_err().kind,
            PoolErrorKind::ReplacementUnderpriced
        ));
        let replacement = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 1_250));
        assert!(pool.insert_validated(replacement, 0).unwrap().replaced.is_some());
        assert_eq!(pool.pending_and_queued_txn_count(), (2, 0));
    }

    #[test]
    fn nonce_free_removal_and_iteration_are_independent() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let high = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 20, 1_000));
        let low = valid_pool_transaction(signed_nonce_free_tx(&signer, 2, 10, 1_000));
        let high_hash = *high.hash();
        let low_hash = *low.hash();
        pool.insert_validated(high, 0).unwrap();
        pool.insert_validated(low, 0).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 0);
        let invalidated = pool.get(&high_hash).unwrap();
        best.mark_invalid(&invalidated, InvalidPoolTransactionError::Underpriced);
        assert_eq!(best.next().map(|tx| *tx.hash()), Some(low_hash));

        assert_eq!(pool.remove_transactions_and_descendants(&[high_hash]).len(), 1);
        assert!(pool.get(&low_hash).is_some());
        assert_eq!(pool.prune_mined(&[low_hash]).removed.len(), 1);
        assert!(pool.all_transactions().is_empty());
    }

    #[test]
    fn nonce_free_expiry_removes_due_transactions_and_keeps_future_entries() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let due = valid_pool_transaction(signed_nonce_free_tx(&signer, 10, 20, 1_000));
        let future = valid_pool_transaction(signed_nonce_free_tx(&signer, 11, 10, 1_000));
        let due_hash = *due.hash();
        let future_hash = *future.hash();
        pool.insert_validated(due, 0).unwrap();
        pool.insert_validated(future, 0).unwrap();

        let removed = pool.remove_expired_nonce_free(10);

        assert_eq!(removed.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![due_hash]);
        assert!(pool.get(&due_hash).is_none());
        assert!(pool.get(&future_hash).is_some());
        assert_eq!(pool.pending_and_queued_txn_count(), (1, 0));
    }

    #[test]
    fn remove_by_sender_cleans_nonce_free_and_finite_channel_indexes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let nonce_free = valid_pool_transaction(signed_nonce_free_tx(&signer, 10, 20, 1_000));
        let channel = valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_000));
        let nonce_free_hash = *nonce_free.hash();
        let channel_hash = *channel.hash();
        pool.insert_validated(nonce_free, 0).unwrap();
        pool.insert_validated(channel, 0).unwrap();

        let removed = pool.remove_transactions_by_sender(signer.address());

        assert_eq!(removed.len(), 2);
        assert!(pool.get(&nonce_free_hash).is_none());
        assert!(pool.get(&channel_hash).is_none());
        assert!(pool.all_transactions().is_empty());
        assert!(pool.unique_senders().is_empty());
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
        let lanes: HashMap<_, _> = [(
            lane_id,
            NonceLane {
                next_nonce: u64::MAX,
                transactions: BTreeMap::from([(u64::MAX, Arc::clone(&transaction))]),
            },
        )]
        .into_iter()
        .collect();

        let mut best =
            BestTwoDTransactions::new(&lanes, &B256Map::default(), BaseOrdering::coinbase_tip(), 0);
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

    #[test]
    fn under_base_fee_transactions_are_parked_outside_pending() {
        let mut pool = pool_with_config(PoolConfig::default(), 100);
        let signer = signer();
        let channel = valid_pool_transaction(signed_channel_tx(&signer, U256::from(51), 0, 99));
        let nonce_free = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 98));

        let channel_outcome = pool.insert_validated(channel, 0).unwrap();
        let nonce_free_outcome = pool.insert_validated(nonce_free, 0).unwrap();

        assert_eq!(
            channel_outcome.outcome.state,
            AddedTransactionState::Queued(QueuedReason::InsufficientBaseFee)
        );
        assert_eq!(
            nonce_free_outcome.outcome.state,
            AddedTransactionState::Queued(QueuedReason::InsufficientBaseFee)
        );
        assert!(pool.pending_transactions().is_empty());
        assert_eq!(pool.basefee_transactions().len(), 2);
        assert_eq!(pool.pending_and_queued_txn_count(), (0, 2));
        assert!(pool.best_transactions(BaseOrdering::coinbase_tip(), 100).next().is_none());
    }

    #[test]
    fn base_fee_updates_demote_and_promote_entire_channel_run() {
        let mut pool = pool_with_config(PoolConfig::default(), 50);
        let signer = signer();
        let head = valid_pool_transaction(signed_channel_tx(&signer, U256::from(52), 0, 100));
        let next = valid_pool_transaction(signed_channel_tx(&signer, U256::from(52), 1, 200));
        let head_hash = *head.hash();
        let next_hash = *next.hash();
        pool.insert_validated(head, 0).unwrap();
        pool.insert_validated(next, 0).unwrap();

        let raised = pool.set_base_fee(150);
        assert_eq!(
            raised.demoted.iter().map(|(tx, pool)| (*tx.hash(), *pool)).collect::<Vec<_>>(),
            vec![(head_hash, SubPool::BaseFee), (next_hash, SubPool::Queued)]
        );
        assert!(raised.promoted.is_empty());
        assert_eq!(pool.subpool_counts(), (0, 1, 1));

        let lowered = pool.set_base_fee(50);
        assert_eq!(
            lowered.promoted.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![head_hash, next_hash]
        );
        assert!(lowered.demoted.is_empty());
        assert_eq!(pool.subpool_counts(), (2, 0, 0));
    }

    #[test]
    fn pending_count_limit_evicts_the_lowest_fee_lane() {
        let config = PoolConfig {
            pending_limit: SubPoolLimit::new(1, usize::MAX),
            max_account_slots: usize::MAX,
            ..PoolConfig::default()
        };
        let mut pool = pool_with_config(config, 0);
        let signer = signer();
        let low = valid_pool_transaction(signed_channel_tx(&signer, U256::from(53), 0, 100));
        let high = valid_pool_transaction(signed_channel_tx(&signer, U256::from(54), 0, 200));
        let low_hash = *low.hash();
        let high_hash = *high.hash();
        pool.insert_validated(low, 0).unwrap();
        pool.insert_validated(high, 0).unwrap();

        let discarded = pool.enforce_limits();

        assert_eq!(discarded.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![low_hash]);
        assert!(pool.get(&low_hash).is_none());
        assert!(pool.get(&high_hash).is_some());
        assert_eq!(pool.subpool_counts(), (1, 0, 0));
    }

    #[test]
    fn pending_byte_limit_evicts_the_lowest_fee_lane() {
        let signer = signer();
        let low = valid_pool_transaction(signed_channel_tx(&signer, U256::from(60), 0, 100));
        let high = valid_pool_transaction(signed_channel_tx(&signer, U256::from(61), 0, 200));
        let config = PoolConfig {
            pending_limit: SubPoolLimit::new(
                usize::MAX,
                low.encoded_length() + high.encoded_length() - 1,
            ),
            max_account_slots: usize::MAX,
            ..PoolConfig::default()
        };
        let mut pool = pool_with_config(config, 0);
        let low_hash = *low.hash();
        pool.insert_validated(low, 0).unwrap();
        pool.insert_validated(high, 0).unwrap();

        let discarded = pool.enforce_limits();

        assert_eq!(discarded.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![low_hash]);
        assert_eq!(pool.subpool_counts(), (1, 0, 0));
    }

    #[test]
    fn queued_count_limit_evicts_the_lowest_fee_lane() {
        let config = PoolConfig {
            queued_limit: SubPoolLimit::new(1, usize::MAX),
            max_account_slots: usize::MAX,
            ..PoolConfig::default()
        };
        let mut pool = pool_with_config(config, 0);
        let signer = signer();
        let low = valid_pool_transaction(signed_channel_tx(&signer, U256::from(62), 2, 100));
        let high = valid_pool_transaction(signed_channel_tx(&signer, U256::from(63), 2, 200));
        let low_hash = *low.hash();
        pool.insert_validated(low, 0).unwrap();
        pool.insert_validated(high, 0).unwrap();

        let discarded = pool.enforce_limits();

        assert_eq!(discarded.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![low_hash]);
        assert_eq!(pool.subpool_counts(), (0, 0, 1));
    }

    #[test]
    fn basefee_limit_is_enforced_independently() {
        let config = PoolConfig {
            basefee_limit: SubPoolLimit::new(1, usize::MAX),
            max_account_slots: usize::MAX,
            ..PoolConfig::default()
        };
        let mut pool = pool_with_config(config, 300);
        let signer = signer();
        let low = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 100));
        let high = valid_pool_transaction(signed_nonce_free_tx(&signer, 2, 0, 200));
        let low_hash = *low.hash();
        pool.insert_validated(low, 0).unwrap();
        pool.insert_validated(high, 0).unwrap();

        let discarded = pool.enforce_limits();

        assert_eq!(discarded.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![low_hash]);
        assert_eq!(pool.subpool_counts(), (0, 1, 0));
    }

    #[test]
    fn queued_byte_limit_removes_a_lane_and_its_descendants() {
        let signer = signer();
        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(55), 2, 100));
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(55), 3, 200));
        let max_size = first.encoded_length() + second.encoded_length() - 1;
        let config = PoolConfig {
            queued_limit: SubPoolLimit::new(usize::MAX, max_size),
            max_account_slots: usize::MAX,
            ..PoolConfig::default()
        };
        let mut pool = pool_with_config(config, 0);
        let first_hash = *first.hash();
        let second_hash = *second.hash();
        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(second, 0).unwrap();

        let discarded = pool.enforce_limits();

        assert_eq!(discarded.len(), 2);
        assert!(discarded.iter().any(|tx| *tx.hash() == first_hash));
        assert!(discarded.iter().any(|tx| *tx.hash() == second_hash));
        assert!(pool.all_transactions().is_empty());
    }

    #[test]
    fn physical_sender_slots_apply_to_external_but_not_local_transactions() {
        let config = PoolConfig { max_account_slots: 1, ..PoolConfig::default() };
        let signer = signer();
        let mut external_pool = pool_with_config(config.clone(), 0);
        external_pool
            .insert_validated(
                valid_pool_transaction(signed_channel_tx(&signer, U256::from(56), 0, 100)),
                0,
            )
            .unwrap();
        let over = valid_pool_transaction(signed_channel_tx(&signer, U256::from(57), 0, 100));
        assert!(matches!(
            external_pool.insert_validated(over, 0).unwrap_err().kind,
            PoolErrorKind::SpammerExceededCapacity(address) if address == signer.address()
        ));

        let mut local_pool = pool_with_config(config, 0);
        for nonce_key in [U256::from(58), U256::from(59)] {
            let mut transaction =
                valid_pool_transaction(signed_channel_tx(&signer, nonce_key, 0, 100));
            transaction.origin = TransactionOrigin::Local;
            local_pool.insert_validated(transaction, 0).unwrap();
        }
        assert_eq!(local_pool.all_transactions().len(), 2);
    }
}
