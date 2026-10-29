//! Index of the EIP-7702 authorities that protocol-pool transactions authorize.

use std::collections::VecDeque;

use alloy_primitives::{
    Address, TxHash,
    map::{AddressMap, B256Map, B256Set},
};

/// The EIP-7702 authorities of pooled protocol transactions, indexed both ways so a
/// transaction's references can be removed when it leaves the pool.
///
/// Each hash is recorded at most once. The pool facade records an admission and forgets every
/// removal it performs. The protocol pool's own capacity evictions are not reported, so every
/// [`Self::record`] also reconciles the [`Self::RECONCILED_PER_RECORD`] oldest records against
/// the pool, rotating live ones to the back and forgetting the rest. Each record therefore
/// revisits every older one within a bounded number of later records, which keeps stale
/// records bounded by a constant multiple of live ones without canonical progress or a
/// pool-wide scan. Live records are never dropped.
#[derive(Debug, Default)]
pub struct ProtocolAuthorizations {
    by_authority: AddressMap<B256Set>,
    /// Each recorded hash's authorities and the sequence number of its live `order` entry.
    by_hash: B256Map<(u64, Vec<Address>)>,
    /// Records in reconciliation order. An entry whose sequence number no longer matches
    /// `by_hash` belongs to a forgotten record and is dropped when reached.
    order: VecDeque<(u64, TxHash)>,
    next_sequence: u64,
}

impl ProtocolAuthorizations {
    /// Oldest records each [`Self::record`] checks against the pool. Two per record outpace
    /// the one record it adds, so the reconciliation backlog shrinks while stale records exist.
    pub const RECONCILED_PER_RECORD: usize = 2;

    /// Records that transaction `hash` authorizes `authorities`, then reconciles the oldest
    /// records against `is_pooled`. A hash already recorded is left unchanged.
    pub fn record(
        &mut self,
        hash: TxHash,
        authorities: Vec<Address>,
        is_pooled: impl Fn(&TxHash) -> bool,
    ) {
        if authorities.is_empty() || self.by_hash.contains_key(&hash) {
            return;
        }
        for authority in &authorities {
            self.by_authority.entry(*authority).or_default().insert(hash);
        }
        let sequence = self.push_order(hash);
        self.by_hash.insert(hash, (sequence, authorities));
        self.reconcile_oldest(is_pooled);
    }

    fn push_order(&mut self, hash: TxHash) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.order.push_back((sequence, hash));
        sequence
    }

    /// Checks the oldest records, keeping live ones at the back and forgetting the others.
    fn reconcile_oldest(&mut self, is_pooled: impl Fn(&TxHash) -> bool) {
        for _ in 0..Self::RECONCILED_PER_RECORD {
            let Some((sequence, hash)) = self.order.pop_front() else {
                return;
            };
            if self.by_hash.get(&hash).is_none_or(|(live, _)| *live != sequence) {
                continue;
            }
            if is_pooled(&hash) {
                let sequence = self.push_order(hash);
                if let Some(entry) = self.by_hash.get_mut(&hash) {
                    entry.0 = sequence;
                }
            } else {
                self.forget(&hash);
            }
        }
    }

    /// Removes every reference to transaction `hash`.
    pub fn forget(&mut self, hash: &TxHash) {
        for authority in
            self.by_hash.remove(hash).into_iter().flat_map(|(_, authorities)| authorities)
        {
            if let Some(hashes) = self.by_authority.get_mut(&authority) {
                hashes.remove(hash);
                if hashes.is_empty() {
                    self.by_authority.remove(&authority);
                }
            }
        }
    }

    /// Returns whether a transaction `is_pooled` reports authorizes `authority`. The check
    /// stops at the first pooled reference and forgets the stale ones it passed, so repeated
    /// queries never rescan live references.
    pub fn retain_pooled_for(
        &mut self,
        authority: &Address,
        is_pooled: impl Fn(&TxHash) -> bool,
    ) -> bool {
        let Some(hashes) = self.by_authority.get(authority) else {
            return false;
        };
        let mut stale = Vec::new();
        let mut found = false;
        for hash in hashes {
            if is_pooled(hash) {
                found = true;
                break;
            }
            stale.push(*hash);
        }
        for hash in &stale {
            self.forget(hash);
        }
        found
    }

    /// Forgets every recorded hash `is_pooled` no longer reports.
    pub fn retain_pooled(&mut self, is_pooled: impl Fn(&TxHash) -> bool) {
        let gone: Vec<_> = self.by_hash.keys().copied().filter(|hash| !is_pooled(hash)).collect();
        for hash in &gone {
            self.forget(hash);
        }
    }

    /// Returns how many transactions are recorded.
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Returns how many entries await reconciliation, including those of forgotten records.
    pub fn backlog(&self) -> usize {
        self.order.len()
    }

    /// Returns whether no transaction is recorded.
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Returns how many recorded transactions authorize `authority`.
    pub fn authorizations_of(&self, authority: &Address) -> usize {
        self.by_authority.get(authority).map_or(0, B256Set::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_is_recorded_once_and_forgetting_it_clears_every_authority() {
        let mut index = ProtocolAuthorizations::default();
        let [first, second] = [Address::repeat_byte(1), Address::repeat_byte(2)];
        let hash = TxHash::repeat_byte(9);

        index.record(hash, vec![first, second], |_| true);
        index.record(hash, vec![first, second], |_| true);
        assert_eq!((index.len(), index.authorizations_of(&first)), (1, 1));

        index.forget(&hash);
        assert!(index.is_empty());
        assert_eq!(index.authorizations_of(&second), 0);
    }

    #[test]
    fn reads_drop_hashes_the_pool_no_longer_holds() {
        let mut index = ProtocolAuthorizations::default();
        let [authority, other] = [Address::repeat_byte(1), Address::repeat_byte(2)];
        let [evicted, pooled] = [TxHash::repeat_byte(1), TxHash::repeat_byte(2)];
        index.record(evicted, vec![authority], |_| true);
        index.record(pooled, vec![other], |_| true);

        assert!(!index.retain_pooled_for(&authority, |hash| *hash == pooled));
        assert!(index.retain_pooled_for(&other, |hash| *hash == pooled));
        assert_eq!(index.len(), 1);
        index.retain_pooled(|_| false);
        assert!(index.is_empty());
    }

    #[test]
    fn records_reconcile_evicted_ones_without_dropping_live_ones() {
        let mut index = ProtocolAuthorizations::default();
        let live = TxHash::repeat_byte(0xff);
        index.record(live, vec![Address::repeat_byte(0xff)], |_| true);

        // Each new record evicts the previous one, as a full pool would; none is ever read.
        for byte in 0..200u8 {
            let hash = TxHash::repeat_byte(byte);
            index.record(hash, vec![Address::repeat_byte(byte)], |candidate| {
                *candidate == live || *candidate == hash
            });
        }

        assert!(index.len() <= 3, "stale records accumulated: {}", index.len());
        assert!(index.backlog() <= 6, "backlog grew: {}", index.backlog());
        assert_eq!(index.authorizations_of(&Address::repeat_byte(0xff)), 1);
    }

    #[test]
    fn an_authority_query_stops_at_the_first_pooled_reference() {
        let mut index = ProtocolAuthorizations::default();
        let authority = Address::repeat_byte(1);
        for byte in 0..32u8 {
            index.record(TxHash::repeat_byte(byte), vec![authority], |_| true);
        }
        let checks = std::cell::Cell::new(0);

        assert!(index.retain_pooled_for(&authority, |_| {
            checks.set(checks.get() + 1);
            true
        }));

        assert_eq!(checks.get(), 1);
        assert_eq!(index.authorizations_of(&authority), 32);
    }
}
