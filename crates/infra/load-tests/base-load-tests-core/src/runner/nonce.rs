use std::collections::{BTreeSet, HashMap};

use alloy_primitives::Address;
use tracing::{debug, warn};

use crate::{rpc::RpcClient, utils::Result};

/// Tracks nonces per account and detects gaps for healing.
#[derive(Debug)]
pub struct NonceTracker {
    expected: HashMap<Address, u64>,
    pending: HashMap<Address, BTreeSet<u64>>,
    confirmed: HashMap<Address, u64>,
    reusable: HashMap<Address, BTreeSet<u64>>,
}

impl NonceTracker {
    /// Creates a new nonce tracker.
    pub fn new() -> Self {
        Self {
            expected: HashMap::new(),
            pending: HashMap::new(),
            confirmed: HashMap::new(),
            reusable: HashMap::new(),
        }
    }

    /// Initializes an account with its current on-chain nonce.
    pub fn init_account(&mut self, address: Address, nonce: u64) {
        self.expected.insert(address, nonce);
        self.pending.insert(address, BTreeSet::new());
        self.confirmed.insert(address, nonce);
        self.reusable.insert(address, BTreeSet::new());
    }

    /// Allocates the next nonce for an account.
    pub fn allocate(&mut self, address: &Address) -> Option<u64> {
        if let Some(reusable) = self.reusable.get_mut(address)
            && let Some(reused_nonce) = reusable.pop_first()
        {
            if let Some(pending) = self.pending.get_mut(address) {
                pending.insert(reused_nonce);
            }
            return Some(reused_nonce);
        }

        let expected = self.expected.get_mut(address)?;
        let allocated = *expected;
        *expected += 1;

        if let Some(pending) = self.pending.get_mut(address) {
            pending.insert(allocated);
        }

        Some(allocated)
    }

    /// Marks a nonce as confirmed.
    pub fn confirm(&mut self, address: &Address, nonce: u64) {
        if let Some(pending) = self.pending.get_mut(address) {
            pending.remove(&nonce);
        }
        if let Some(reusable) = self.reusable.get_mut(address) {
            reusable.remove(&nonce);
        }

        if let Some(confirmed) = self.confirmed.get_mut(address)
            && nonce > *confirmed
        {
            *confirmed = nonce;
        }
    }

    /// Marks a nonce as failed, making it available for reuse.
    pub fn fail(&mut self, address: &Address, nonce: u64) {
        if let Some(pending) = self.pending.get_mut(address) {
            pending.remove(&nonce);
        }
        if let Some(reusable) = self.reusable.get_mut(address) {
            reusable.insert(nonce);
        }
    }

    /// Finds nonce gaps for an account (nonces that failed and need retry).
    pub fn find_gaps(&self, address: &Address) -> Vec<u64> {
        let Some(confirmed) = self.confirmed.get(address) else {
            return Vec::new();
        };
        let Some(expected) = self.expected.get(address) else {
            return Vec::new();
        };
        let Some(pending) = self.pending.get(address) else {
            return Vec::new();
        };

        let mut gaps = Vec::new();
        for nonce in *confirmed..*expected {
            if !pending.contains(&nonce) {
                gaps.push(nonce);
            }
        }

        gaps
    }

    /// Heals nonce gaps by querying the chain and resetting expected nonces.
    pub async fn heal(&mut self, address: &Address, client: &RpcClient) -> Result<Vec<u64>> {
        let on_chain_nonce = client.get_nonce(*address).await?;

        let gaps = self.find_gaps(address);
        if gaps.is_empty() {
            return Ok(Vec::new());
        }

        let expected = self.expected.get_mut(address);
        let pending = self.pending.get_mut(address);
        let reusable = self.reusable.get_mut(address);

        if let (Some(expected), Some(pending), Some(reusable)) = (expected, pending, reusable) {
            let stale: Vec<u64> =
                pending.iter().filter(|&&n| n < on_chain_nonce).copied().collect();
            for nonce in &stale {
                pending.remove(nonce);
                debug!(address = %address, nonce, "removed stale pending nonce");
            }

            // Nonces below on-chain nonce are already consumed and should never be retried.
            reusable.retain(|n| *n >= on_chain_nonce);

            if *expected < on_chain_nonce {
                warn!(
                    address = %address,
                    expected = *expected,
                    on_chain = on_chain_nonce,
                    "nonce jumped ahead, updating"
                );
                *expected = on_chain_nonce;
            }

            for gap in &gaps {
                if *gap >= on_chain_nonce && !pending.contains(gap) {
                    reusable.insert(*gap);
                }
            }
        }

        if let Some(confirmed) = self.confirmed.get_mut(address)
            && on_chain_nonce > *confirmed
        {
            *confirmed = on_chain_nonce;
        }

        Ok(gaps)
    }

    /// Returns pending nonce count for an account.
    pub fn pending_count(&self, address: &Address) -> usize {
        self.pending.get(address).map(|p| p.len()).unwrap_or(0)
    }
}

impl Default for NonceTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::NonceTracker;

    #[test]
    fn failed_nonce_is_reused_before_new_nonce() {
        let mut tracker = NonceTracker::new();
        let address = Address::repeat_byte(1);
        tracker.init_account(address, 10);

        let first = tracker.allocate(&address).expect("first nonce");
        let second = tracker.allocate(&address).expect("second nonce");
        assert_eq!(first, 10);
        assert_eq!(second, 11);

        tracker.fail(&address, first);

        let reused = tracker.allocate(&address).expect("reused nonce");
        let next_new = tracker.allocate(&address).expect("next fresh nonce");
        assert_eq!(reused, 10);
        assert_eq!(next_new, 12);
    }

    #[test]
    fn reusable_nonce_is_cleared_after_confirm() {
        let mut tracker = NonceTracker::new();
        let address = Address::repeat_byte(2);
        tracker.init_account(address, 1);

        let nonce = tracker.allocate(&address).expect("allocated");
        tracker.fail(&address, nonce);
        tracker.confirm(&address, nonce);

        let next = tracker.allocate(&address).expect("next nonce");
        assert_eq!(next, 2);
    }
}
