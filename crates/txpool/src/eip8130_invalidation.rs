//! State-diff based invalidation for EIP-8130 Account Abstraction transactions.
//!
//! Tracks which storage slots each pending AA transaction depends on, so that
//! when a block's state diff reports changed slots, the affected transactions
//! can be efficiently identified and evicted from the mempool.
//!
//! # Wiring
//!
//! [`maintain_eip8130_invalidation`] is the main maintenance loop. It listens
//! for [`CanonStateNotification`] events, extracts storage changes, and removes
//! invalidated transactions from the pool. The shared
//! [`Eip8130InvalidationIndex`] is populated by `OpTransactionValidator` during
//! validation and read by the maintenance task.
//!
//! TODO: When Block Access Lists (BAL) become available, pass them to the
//! index for mass invalidation instead of relying solely on state diffs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, U256};
use base_alloy_consensus::{
    ACCOUNT_CONFIG_ADDRESS, AccountChangeEntry, NONCE_MANAGER_ADDRESS, TxEip8130, nonce_slot,
    owner_config_slot, parse_delegate_auth,
};
use futures::StreamExt;
use parking_lot::RwLock;
use reth_node_api::NodePrimitives;
use reth_provider::CanonStateNotification;
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use tokio_stream::wrappers::BroadcastStream;
use tracing::{debug, trace, warn};

use crate::Eip8130Pool;

/// A (contract address, storage slot) pair that an AA transaction depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InvalidationKey {
    /// The contract whose storage is being watched.
    pub address: Address,
    /// The specific storage slot within that contract.
    pub slot: B256,
}

/// Index that maps invalidation keys to the set of transaction hashes that
/// depend on them. Also tracks per-payer pending counts for sponsored AA txs.
#[derive(Debug, Default)]
pub struct Eip8130InvalidationIndex {
    key_to_txs: HashMap<InvalidationKey, HashSet<B256>>,
    tx_to_keys: HashMap<B256, HashSet<InvalidationKey>>,
    tx_to_payer: HashMap<B256, Address>,
    payer_counts: HashMap<Address, usize>,
}

impl Eip8130InvalidationIndex {
    /// Inserts a transaction and its invalidation keys into the index.
    ///
    /// If `payer` is `Some`, tracks the payer for pending-count enforcement.
    /// Pass `Some(payer)` for sponsored AA txs where payer != sender.
    pub fn insert(
        &mut self,
        tx_hash: B256,
        keys: HashSet<InvalidationKey>,
        payer: Option<Address>,
    ) {
        for key in &keys {
            self.key_to_txs.entry(*key).or_default().insert(tx_hash);
        }
        self.tx_to_keys.insert(tx_hash, keys);
        if let Some(addr) = payer {
            self.tx_to_payer.insert(tx_hash, addr);
            *self.payer_counts.entry(addr).or_default() += 1;
        }
    }

    /// Returns the set of transaction hashes affected by the given key.
    pub fn lookup(&self, key: &InvalidationKey) -> Option<&HashSet<B256>> {
        self.key_to_txs.get(key)
    }

    /// Removes a transaction from the index, cleaning up all associated keys.
    pub fn remove(&mut self, tx_hash: &B256) {
        if let Some(keys) = self.tx_to_keys.remove(tx_hash) {
            for key in &keys {
                if let Some(txs) = self.key_to_txs.get_mut(key) {
                    txs.remove(tx_hash);
                    if txs.is_empty() {
                        self.key_to_txs.remove(key);
                    }
                }
            }
        }
        if let Some(payer) = self.tx_to_payer.remove(tx_hash) {
            if let Some(count) = self.payer_counts.get_mut(&payer) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.payer_counts.remove(&payer);
                }
            }
        }
    }

    /// Returns all transaction hashes invalidated by any of the given keys.
    pub fn invalidated_by(&self, keys: &[InvalidationKey]) -> HashSet<B256> {
        let mut result = HashSet::new();
        for key in keys {
            if let Some(txs) = self.key_to_txs.get(key) {
                result.extend(txs);
            }
        }
        result
    }

    /// Returns the number of pending sponsored txs for a given payer.
    pub fn payer_pending_count(&self, payer: &Address) -> usize {
        self.payer_counts.get(payer).copied().unwrap_or(0)
    }

    /// Returns the number of tracked transactions.
    pub fn len(&self) -> usize {
        self.tx_to_keys.len()
    }

    /// Returns true if there are no tracked transactions.
    pub fn is_empty(&self) -> bool {
        self.tx_to_keys.is_empty()
    }

    /// Returns all tracked transaction hashes.
    pub fn tracked_tx_hashes(&self) -> impl Iterator<Item = &B256> {
        self.tx_to_keys.keys()
    }

    /// Removes all transactions whose hashes are NOT in the given live set.
    ///
    /// Returns the number of stale entries pruned.
    pub fn prune_stale(&mut self, live: &HashSet<B256>) -> usize {
        let stale: Vec<B256> =
            self.tx_to_keys.keys().filter(|hash| !live.contains(*hash)).copied().collect();

        let count = stale.len();
        for hash in stale {
            self.remove(&hash);
        }
        count
    }
}

/// Computes the set of storage slots that this AA transaction depends on.
///
/// A state change to any of these slots should trigger re-validation or eviction.
///
/// `resolved_sender` is the effective sender address (ecrecovered for EOA mode,
/// `tx.from` for configured mode). This must be passed because `tx.from` is
/// `Address::ZERO` in EOA mode.
///
/// When available, pass the resolved `sender_owner_id` and `payer_owner_id`
/// from validation to track the exact owner config slots. Falls back to a
/// hash-based proxy when `None`.
///
pub fn compute_invalidation_keys(
    tx: &TxEip8130,
    resolved_sender: Address,
    resolved_sender_owner_id: Option<B256>,
    resolved_payer_owner_id: Option<B256>,
) -> HashSet<InvalidationKey> {
    let mut keys = HashSet::new();
    let sender = resolved_sender;

    // 1. Nonce slot — the sender's 2D nonce at (sender, nonce_key)
    let nonce_key_slot = nonce_slot(sender, U256::from(tx.nonce_key));
    keys.insert(InvalidationKey { address: NONCE_MANAGER_ADDRESS, slot: nonce_key_slot });

    // 2. Sender owner config slot — use the resolved owner_id if available
    //    (from validation), otherwise fall back to keccak256(sender_auth) as
    //    a proxy. The resolved owner_id gives us the exact storage slot.
    if !tx.sender_auth.is_empty() {
        let owner_id = resolved_sender_owner_id
            .unwrap_or_else(|| alloy_primitives::keccak256(&tx.sender_auth));
        let config_slot = owner_config_slot(sender, owner_id);
        keys.insert(InvalidationKey { address: ACCOUNT_CONFIG_ADDRESS, slot: config_slot });
    }

    // 3. Payer owner config — if there's a separate payer, their owner
    //    authorization can be revoked, invalidating the tx.
    if let Some(payer) = tx.payer.filter(|payer| *payer != sender) {
        if tx.payer_auth.is_empty() {
            // `eth_estimateGas` may omit payer auth; no payer owner slot dependency yet.
        } else {
            let payer_owner_id = resolved_payer_owner_id
                .unwrap_or_else(|| alloy_primitives::keccak256(&tx.payer_auth));
            let payer_config_slot = owner_config_slot(payer, payer_owner_id);
            keys.insert(InvalidationKey {
                address: ACCOUNT_CONFIG_ADDRESS,
                slot: payer_config_slot,
            });
        }
    }

    // Delegate-native auth also depends on the delegate account's nested owner
    // table. Any config change on that account bumps the packed sequence slot.
    for auth in [&tx.sender_auth, &tx.payer_auth] {
        if let Some(delegate_account) = delegate_account_from_auth(auth) {
            let delegate_seq_slot = base_alloy_consensus::sequence_base_slot(delegate_account);
            keys.insert(InvalidationKey {
                address: ACCOUNT_CONFIG_ADDRESS,
                slot: delegate_seq_slot,
            });
        }
    }

    // 4. Account changes — each create entry depends on the target address having
    //    no code, and each config change depends on the sender's lock state and
    //    change sequence.
    for change in &tx.account_changes {
        match change {
            AccountChangeEntry::Create(create) => {
                let deployer_hash = alloy_primitives::keccak256(
                    [
                        sender.as_slice(),
                        create.user_salt.as_slice(),
                        &alloy_primitives::keccak256(&create.bytecode).0,
                    ]
                    .concat(),
                );
                keys.insert(InvalidationKey { address: sender, slot: deployer_hash });
            }
            AccountChangeEntry::ConfigChange(_cc) => {
                let lock_key_slot = base_alloy_consensus::lock_slot(sender);
                keys.insert(InvalidationKey {
                    address: ACCOUNT_CONFIG_ADDRESS,
                    slot: lock_key_slot,
                });

                // Both multichain and local sequences are packed into a single
                // slot, so watching the base slot covers both chain_id variants.
                // This also covers authorizer invalidation: if the authorizer
                // is revoked (a config change on the same account), the
                // sequence bumps and this slot changes.
                let seq_slot = base_alloy_consensus::sequence_base_slot(sender);
                keys.insert(InvalidationKey { address: ACCOUNT_CONFIG_ADDRESS, slot: seq_slot });
            }
            AccountChangeEntry::Delegation(_) => {}
        }
    }

    // Config-change authorizers can also use delegate auth. Track the delegate
    // account sequence slot so authorizer revocations invalidate pending txs.
    for change in &tx.account_changes {
        let AccountChangeEntry::ConfigChange(cc) = change else {
            continue;
        };
        if let Some(delegate_account) = delegate_account_from_auth(&cc.authorizer_auth) {
            let delegate_seq_slot = base_alloy_consensus::sequence_base_slot(delegate_account);
            keys.insert(InvalidationKey {
                address: ACCOUNT_CONFIG_ADDRESS,
                slot: delegate_seq_slot,
            });
        }
    }

    keys
}

/// Given a set of FAL entries (touched storage slots from a block), finds
/// all pending AA transactions that should be invalidated and returns their
/// hashes.
pub fn process_fal(fal: &[(Address, B256)], index: &Eip8130InvalidationIndex) -> HashSet<B256> {
    let mut result = HashSet::new();
    for &(address, slot) in fal {
        let key = InvalidationKey { address, slot };
        if let Some(txs) = index.lookup(&key) {
            result.extend(txs);
        }
    }
    result
}

/// How often (in blocks) the stale-entry pruning pass runs.
const PRUNE_INTERVAL_BLOCKS: u64 = 16;

fn delegate_account_from_auth(auth: &[u8]) -> Option<Address> {
    parse_delegate_auth(auth).map(|parsed| parsed.delegate_account)
}

/// Maintenance loop that evicts EIP-8130 transactions from the pool when the
/// storage slots they depend on change.
///
/// Listens to [`CanonStateNotification`] events and, for each committed block,
/// extracts storage changes for the two AA system contracts
/// ([`ACCOUNT_CONFIG_ADDRESS`] and [`NONCE_MANAGER_ADDRESS`]). Matching
/// transactions are removed from both the pool and the shared invalidation
/// index.
///
/// When `NONCE_MANAGER_ADDRESS` storage slots change, the 2D nonce pool is
/// also updated: the affected sequence lanes advance their `next_nonce` and
/// stale transactions are pruned.
pub async fn maintain_eip8130_invalidation<P, N, T>(
    pool: P,
    eip8130_pool: Arc<Eip8130Pool<T>>,
    mut events: BroadcastStream<CanonStateNotification<N>>,
    index: Arc<RwLock<Eip8130InvalidationIndex>>,
) where
    P: TransactionPool + 'static,
    T: PoolTransaction + 'static,
    N: NodePrimitives,
{
    let mut blocks_since_prune: u64 = 0;

    loop {
        let notification = match events.next().await {
            Some(Ok(notification)) => notification,
            Some(Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n))) => {
                warn!(
                    missed = n,
                    "canon state stream lagged, some blocks were not checked for AA invalidation"
                );
                continue;
            }
            None => break,
        };

        blocks_since_prune += 1;

        let tip = notification.tip();
        let block_timestamp = tip.timestamp();

        let committed = notification.committed();
        let execution_outcome = committed.execution_outcome();

        let mut touched: Vec<(Address, B256)> = Vec::new();
        let mut nonce_slot_changes: Vec<(B256, U256)> = Vec::new();
        let mut config_slots: Vec<B256> = Vec::new();

        for (addr, acc) in execution_outcome.bundle_accounts_iter() {
            if addr == NONCE_MANAGER_ADDRESS {
                for (key, slot) in acc.storage.iter() {
                    let slot_key = B256::from(*key);
                    touched.push((addr, slot_key));
                    nonce_slot_changes.push((slot_key, slot.present_value));
                }
            } else if addr == ACCOUNT_CONFIG_ADDRESS {
                for (key, _slot) in acc.storage.iter() {
                    let slot_key = B256::from(*key);
                    touched.push((addr, slot_key));
                    config_slots.push(slot_key);
                }
            }
        }

        // Invalidate cached throughput tiers when lock slots change.
        if !config_slots.is_empty() {
            let n = eip8130_pool.invalidate_tiers_for_lock_slots(&config_slots);
            if n > 0 {
                debug!(
                    invalidated = n,
                    config_slots = config_slots.len(),
                    "invalidated sender throughput tiers from lock slot changes"
                );
            }
        }

        // Update 2D nonce pool when nonce storage slots change.
        // The pool maintains a reverse index from storage slots to sequence
        // IDs, so we can efficiently find and update the right lanes.
        if !nonce_slot_changes.is_empty() {
            let mut nonce_pruned = 0usize;
            for (slot_key, new_value) in &nonce_slot_changes {
                if let Some(seq_id) = eip8130_pool.seq_id_for_slot(slot_key) {
                    let new_nonce: u64 = new_value.try_into().unwrap_or(u64::MAX);
                    let pruned = eip8130_pool.update_sequence_nonce(&seq_id, new_nonce);
                    nonce_pruned += pruned.len();
                }
            }
            if nonce_pruned > 0 {
                debug!(
                    removal_reason = "nonce_advanced",
                    pruned = nonce_pruned,
                    slots = nonce_slot_changes.len(),
                    "removed AA transactions from mempool after nonce advancement"
                );
            }
        }

        // Skip invalidation index lookup when the index is empty.
        if index.read().is_empty() && touched.is_empty() {
            continue;
        }

        if !touched.is_empty() {
            let invalidated = {
                let mut idx = index.write();
                let invalidated = process_fal(&touched, &idx);
                for tx_hash in &invalidated {
                    idx.remove(tx_hash);
                }
                invalidated
            };

            if invalidated.is_empty() {
                trace!(
                    touched_slots = touched.len(),
                    "AA storage changes did not match any pending transactions"
                );
            } else {
                debug!(
                    removal_reason = "state_invalidation",
                    count = invalidated.len(),
                    touched_slots = touched.len(),
                    "removed invalidated AA transactions from mempool"
                );
                let hash_vec: Vec<B256> = invalidated.iter().copied().collect();
                eip8130_pool.remove_transactions(&hash_vec);
                pool.remove_transactions(invalidated.into_iter().collect());
            }
        }

        // Sweep transactions whose `expiry` timestamp has passed.
        let expired = eip8130_pool.sweep_expired(block_timestamp);
        if !expired.is_empty() {
            debug!(
                removal_reason = "expiry",
                count = expired.len(),
                block_timestamp,
                "removed expired AA transactions from mempool"
            );
            pool.remove_transactions(expired);
        }

        // Periodically prune stale index entries for transactions the pool
        // has already dropped (replaced, capacity eviction, expiry).
        if blocks_since_prune >= PRUNE_INTERVAL_BLOCKS {
            blocks_since_prune = 0;

            let idx_guard = index.read();
            if !idx_guard.is_empty() {
                let live: HashSet<B256> = idx_guard
                    .tracked_tx_hashes()
                    .filter(|hash| pool.get(hash).is_some() || eip8130_pool.contains(hash))
                    .copied()
                    .collect();
                drop(idx_guard);

                let pruned = index.write().prune_stale(&live);
                if pruned > 0 {
                    debug!(pruned, "pruned stale AA invalidation index entries");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256};
    use base_alloy_consensus::{
        ACCOUNT_CONFIG_ADDRESS, AccountChangeEntry, ConfigChangeEntry, CreateEntry,
        DELEGATE_VERIFIER_ADDRESS, NONCE_MANAGER_ADDRESS, OP_AUTHORIZE_OWNER, OwnerChange,
        TxEip8130, nonce_slot,
    };

    use super::*;

    fn make_simple_tx(from: Address, nonce_key: u64) -> TxEip8130 {
        TxEip8130 {
            chain_id: 1,
            from: Some(from),
            nonce_key: U256::from(nonce_key),
            nonce_sequence: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            sender_auth: Bytes::from(vec![0u8; 65]),
            ..Default::default()
        }
    }

    #[test]
    fn index_insert_lookup_remove() {
        let mut index = Eip8130InvalidationIndex::default();
        let tx_hash = B256::repeat_byte(0x01);
        let key = InvalidationKey { address: NONCE_MANAGER_ADDRESS, slot: B256::repeat_byte(0xAA) };

        let mut keys = HashSet::new();
        keys.insert(key);
        index.insert(tx_hash, keys, None);

        assert_eq!(index.len(), 1);
        assert!(index.lookup(&key).unwrap().contains(&tx_hash));

        index.remove(&tx_hash);
        assert!(index.is_empty());
        assert!(index.lookup(&key).is_none());
    }

    #[test]
    fn compute_keys_includes_nonce_slot() {
        let from = Address::repeat_byte(0x42);
        let tx = make_simple_tx(from, 0);
        let keys = compute_invalidation_keys(&tx, from, None, None);

        let expected_slot = nonce_slot(from, U256::ZERO);
        assert!(
            keys.contains(&InvalidationKey { address: NONCE_MANAGER_ADDRESS, slot: expected_slot })
        );
    }

    #[test]
    fn compute_keys_with_config_change() {
        let from = Address::repeat_byte(0x42);
        let tx = TxEip8130 {
            chain_id: 1,
            from: Some(from),
            account_changes: vec![AccountChangeEntry::ConfigChange(ConfigChangeEntry {
                chain_id: 1,
                sequence: 0,
                owner_changes: vec![OwnerChange {
                    change_type: OP_AUTHORIZE_OWNER,
                    verifier: Address::repeat_byte(0x01),
                    owner_id: B256::repeat_byte(0x02),
                    scope: 0,
                }],
                authorizer_auth: Bytes::from(vec![0u8; 65]),
            })],
            sender_auth: Bytes::from(vec![0u8; 65]),
            ..Default::default()
        };

        let keys = compute_invalidation_keys(&tx, from, None, None);
        let lock_key = base_alloy_consensus::lock_slot(from);
        assert!(
            keys.contains(&InvalidationKey { address: ACCOUNT_CONFIG_ADDRESS, slot: lock_key })
        );
    }

    #[test]
    fn compute_keys_include_delegate_sequence_slot_for_sender_auth() {
        let from = Address::repeat_byte(0x42);
        let delegate_account = Address::repeat_byte(0x99);
        let nested_verifier = Address::repeat_byte(0x11);
        let mut sender_auth = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        sender_auth.extend_from_slice(delegate_account.as_slice());
        sender_auth.extend_from_slice(nested_verifier.as_slice());
        sender_auth.extend_from_slice(&[0xAA; 32]);

        let tx = TxEip8130 {
            chain_id: 1,
            from: Some(from),
            sender_auth: Bytes::from(sender_auth),
            ..Default::default()
        };

        let keys = compute_invalidation_keys(&tx, from, None, None);
        let delegate_seq_slot = base_alloy_consensus::sequence_base_slot(delegate_account);
        assert!(keys.contains(&InvalidationKey {
            address: ACCOUNT_CONFIG_ADDRESS,
            slot: delegate_seq_slot
        }));
    }

    #[test]
    fn compute_keys_include_delegate_sequence_slot_for_authorizer_auth() {
        let from = Address::repeat_byte(0x42);
        let delegate_account = Address::repeat_byte(0x55);
        let nested_verifier = Address::repeat_byte(0x77);
        let mut authorizer_auth = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        authorizer_auth.extend_from_slice(delegate_account.as_slice());
        authorizer_auth.extend_from_slice(nested_verifier.as_slice());
        authorizer_auth.extend_from_slice(&[0xBB; 32]);

        let tx = TxEip8130 {
            chain_id: 1,
            from: Some(from),
            sender_auth: Bytes::from(vec![0u8; 65]),
            account_changes: vec![AccountChangeEntry::ConfigChange(ConfigChangeEntry {
                chain_id: 1,
                sequence: 0,
                owner_changes: vec![OwnerChange {
                    change_type: OP_AUTHORIZE_OWNER,
                    verifier: Address::repeat_byte(0x01),
                    owner_id: B256::repeat_byte(0x02),
                    scope: 0,
                }],
                authorizer_auth: Bytes::from(authorizer_auth),
            })],
            ..Default::default()
        };

        let keys = compute_invalidation_keys(&tx, from, None, None);
        let delegate_seq_slot = base_alloy_consensus::sequence_base_slot(delegate_account);
        assert!(keys.contains(&InvalidationKey {
            address: ACCOUNT_CONFIG_ADDRESS,
            slot: delegate_seq_slot
        }));
    }

    #[test]
    fn compute_keys_with_create() {
        let from = Address::repeat_byte(0x42);
        let tx = TxEip8130 {
            chain_id: 1,
            from: Some(from),
            account_changes: vec![AccountChangeEntry::Create(CreateEntry {
                user_salt: B256::repeat_byte(0x01),
                bytecode: Bytes::from(vec![0x60, 0x00]),
                initial_owners: vec![],
            })],
            sender_auth: Bytes::from(vec![0u8; 65]),
            ..Default::default()
        };

        let keys = compute_invalidation_keys(&tx, from, None, None);
        // Should have nonce key + at least one create-related key
        assert!(keys.len() >= 2);
    }

    #[test]
    fn compute_keys_uses_resolved_sender_for_eoa() {
        let resolved = Address::repeat_byte(0xAA);
        let tx = TxEip8130 {
            chain_id: 1,
            from: Some(Address::ZERO),
            sender_auth: Bytes::from(vec![0u8; 65]),
            ..Default::default()
        };
        let keys = compute_invalidation_keys(&tx, resolved, None, None);

        let expected_slot = nonce_slot(resolved, U256::ZERO);
        assert!(
            keys.contains(&InvalidationKey { address: NONCE_MANAGER_ADDRESS, slot: expected_slot })
        );

        let wrong_slot = nonce_slot(Address::ZERO, U256::ZERO);
        assert!(
            !keys.contains(&InvalidationKey { address: NONCE_MANAGER_ADDRESS, slot: wrong_slot })
        );
    }

    #[test]
    fn process_fal_finds_invalidated_txs() {
        let mut index = Eip8130InvalidationIndex::default();

        let from = Address::repeat_byte(0x42);
        let tx = make_simple_tx(from, 0);
        let tx_hash = B256::repeat_byte(0x01);
        let keys = compute_invalidation_keys(&tx, from, None, None);
        index.insert(tx_hash, keys, None);

        let nonce_key_slot = nonce_slot(from, U256::ZERO);
        let fal = vec![(NONCE_MANAGER_ADDRESS, nonce_key_slot)];
        let invalidated = process_fal(&fal, &index);
        assert!(invalidated.contains(&tx_hash));
    }

    #[test]
    fn prune_stale_removes_dead_entries() {
        let mut index = Eip8130InvalidationIndex::default();
        let from = Address::repeat_byte(0x42);

        let tx1 = make_simple_tx(from, 0);
        let hash1 = B256::repeat_byte(0x01);
        index.insert(hash1, compute_invalidation_keys(&tx1, from, None, None), None);

        let tx2 = make_simple_tx(from, 1);
        let hash2 = B256::repeat_byte(0x02);
        index.insert(hash2, compute_invalidation_keys(&tx2, from, None, None), None);

        assert_eq!(index.len(), 2);

        // Only hash1 is still "live" in the pool
        let live: HashSet<B256> = [hash1].into_iter().collect();
        let pruned = index.prune_stale(&live);

        assert_eq!(pruned, 1);
        assert_eq!(index.len(), 1);
        assert!(index.tx_to_keys.contains_key(&hash1));
        assert!(!index.tx_to_keys.contains_key(&hash2));
    }

    #[test]
    fn prune_stale_no_op_when_all_live() {
        let mut index = Eip8130InvalidationIndex::default();
        let from = Address::repeat_byte(0x42);

        let tx1 = make_simple_tx(from, 0);
        let hash1 = B256::repeat_byte(0x01);
        index.insert(hash1, compute_invalidation_keys(&tx1, from, None, None), None);

        let live: HashSet<B256> = [hash1].into_iter().collect();
        let pruned = index.prune_stale(&live);

        assert_eq!(pruned, 0);
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn prune_stale_cleans_key_to_txs_map() {
        let mut index = Eip8130InvalidationIndex::default();
        let from = Address::repeat_byte(0x42);

        let tx = make_simple_tx(from, 0);
        let hash = B256::repeat_byte(0x01);
        let keys = compute_invalidation_keys(&tx, from, None, None);
        index.insert(hash, keys, None);

        let nonce_key =
            InvalidationKey { address: NONCE_MANAGER_ADDRESS, slot: nonce_slot(from, U256::ZERO) };
        assert!(index.lookup(&nonce_key).is_some());

        let live: HashSet<B256> = HashSet::new();
        let pruned = index.prune_stale(&live);

        assert_eq!(pruned, 1);
        assert!(index.is_empty());
        assert!(index.lookup(&nonce_key).is_none());
    }

    #[test]
    fn process_fal_unrelated_slot() {
        let mut index = Eip8130InvalidationIndex::default();

        let from = Address::repeat_byte(0x42);
        let tx = make_simple_tx(from, 0);
        let tx_hash = B256::repeat_byte(0x01);
        let keys = compute_invalidation_keys(&tx, from, None, None);
        index.insert(tx_hash, keys, None);

        let fal = vec![(NONCE_MANAGER_ADDRESS, B256::repeat_byte(0xFF))];
        let invalidated = process_fal(&fal, &index);
        assert!(invalidated.is_empty());
    }

    #[test]
    fn payer_pending_count_tracked() {
        let mut index = Eip8130InvalidationIndex::default();
        let payer = Address::repeat_byte(0xBB);
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);

        index.insert(hash1, HashSet::new(), Some(payer));
        assert_eq!(index.payer_pending_count(&payer), 1);

        index.insert(hash2, HashSet::new(), Some(payer));
        assert_eq!(index.payer_pending_count(&payer), 2);

        index.remove(&hash1);
        assert_eq!(index.payer_pending_count(&payer), 1);

        index.remove(&hash2);
        assert_eq!(index.payer_pending_count(&payer), 0);
    }

    #[test]
    fn self_pay_does_not_track_payer() {
        let mut index = Eip8130InvalidationIndex::default();
        let hash = B256::repeat_byte(0x01);
        index.insert(hash, HashSet::new(), None);
        assert_eq!(index.payer_counts.len(), 0);
    }
}
