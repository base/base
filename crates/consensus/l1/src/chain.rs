use std::collections::{BTreeMap, HashMap};

use alloy_consensus::{Header, Sealed};
use alloy_primitives::B256;

/// Canonical index of verified L1 headers, keyed by number.
///
/// Every header enters through [`Self::extend`] or [`Self::prepend`], which require the batch to
/// hash-link to a header already trusted, so the index only ever holds headers that chain back to
/// a light-client verified head.
#[derive(Debug, Clone)]
pub struct VerifiedChain {
    /// Canonical headers by number.
    pub by_number: BTreeMap<u64, Sealed<Header>>,
    /// Canonical block numbers by hash.
    pub by_hash: HashMap<B256, u64>,
    /// Maximum number of headers kept.
    ///
    /// ponytail: the oldest headers are dropped past this bound and re-fetched from peers on
    /// demand; persist to disk if restarts need to skip that.
    pub max_len: usize,
}

impl VerifiedChain {
    /// Creates an empty index holding at most `max_len` headers.
    pub fn new(max_len: usize) -> Self {
        Self { by_number: BTreeMap::new(), by_hash: HashMap::new(), max_len }
    }

    /// Returns the highest canonical header.
    pub fn tip(&self) -> Option<&Sealed<Header>> {
        self.by_number.last_key_value().map(|(_, header)| header)
    }

    /// Returns the lowest canonical header.
    pub fn lowest(&self) -> Option<&Sealed<Header>> {
        self.by_number.first_key_value().map(|(_, header)| header)
    }

    /// Returns the canonical header with `hash`.
    pub fn by_hash(&self, hash: B256) -> Option<&Sealed<Header>> {
        self.by_hash.get(&hash).and_then(|number| self.by_number.get(number))
    }

    /// Makes `headers` canonical, replacing everything above their parent.
    ///
    /// `headers` must be newest first and hash-linked to a trusted head. Returns `false` without
    /// changes if the batch neither joins the index nor the index is empty; the caller then
    /// fetches further back or calls [`Self::reset`].
    pub fn extend(&mut self, headers: &[Sealed<Header>]) -> bool {
        let Some(oldest) = headers.last() else { return false };
        let joins = self.by_number.is_empty()
            || self
                .by_number
                .get(&(oldest.number.saturating_sub(1)))
                .is_some_and(|parent| parent.hash() == oldest.parent_hash);
        if !joins {
            return false;
        }
        for (_, stale) in self.by_number.split_off(&oldest.number) {
            self.by_hash.remove(&stale.hash());
        }
        self.insert(headers);
        true
    }

    /// Adds `headers` below the lowest header.
    ///
    /// `headers` must be newest first. Returns `false` without changes if the newest is not the
    /// parent of the lowest header.
    pub fn prepend(&mut self, headers: &[Sealed<Header>]) -> bool {
        let joins = match (self.lowest(), headers.first()) {
            (Some(lowest), Some(newest)) => lowest.parent_hash == newest.hash(),
            _ => false,
        };
        if joins {
            self.insert(headers);
        }
        joins
    }

    /// Replaces the index with `headers`, newest first.
    pub fn reset(&mut self, headers: &[Sealed<Header>]) {
        self.by_number.clear();
        self.by_hash.clear();
        self.insert(headers);
    }

    fn insert(&mut self, headers: &[Sealed<Header>]) {
        for header in headers {
            self.by_hash.insert(header.hash(), header.number);
            self.by_number.insert(header.number, header.clone());
        }
        while self.by_number.len() > self.max_len {
            if let Some((_, old)) = self.by_number.pop_first() {
                self.by_hash.remove(&old.hash());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Sealable;

    use super::*;

    fn chain(parent: B256, from: u64, len: u64, salt: u64) -> Vec<Sealed<Header>> {
        let mut parent_hash = parent;
        let mut headers: Vec<_> = (from..from + len)
            .map(|number| {
                let header = Header { number, parent_hash, gas_limit: salt, ..Default::default() }
                    .seal_slow();
                parent_hash = header.hash();
                header
            })
            .collect();
        headers.reverse();
        headers
    }

    #[test]
    fn extends_reorgs_and_prunes() {
        let mut index = VerifiedChain::new(8);
        let base = chain(B256::ZERO, 10, 5, 0);
        assert!(index.extend(&base));
        assert_eq!(index.tip().unwrap().number, 14);

        let unrelated = chain(B256::repeat_byte(1), 15, 2, 0);
        assert!(!index.extend(&unrelated));

        let fork = chain(index.by_number[&12].hash(), 13, 1, 1);
        assert!(index.extend(&fork));
        assert_eq!(index.tip().unwrap().hash(), fork[0].hash());
        assert!(index.by_hash(base[0].hash()).is_none());

        let longer = chain(fork[0].hash(), 14, 10, 0);
        assert!(index.extend(&longer));
        assert_eq!(index.by_number.len(), 8);
        assert_eq!(index.lowest().unwrap().number, 16);
        assert!(index.by_hash(fork[0].hash()).is_none());
    }

    #[test]
    fn prepends_only_linked_parents() {
        let full = chain(B256::ZERO, 0, 6, 0);
        let mut index = VerifiedChain::new(100);
        index.reset(&full[..3]);
        assert!(!index.prepend(&chain(B256::ZERO, 0, 3, 7)));
        assert!(index.prepend(&full[3..]));
        assert_eq!(index.lowest().unwrap().number, 0);
    }
}
