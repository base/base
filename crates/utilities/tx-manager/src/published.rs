//! Hashes retained across send errors and cancellation.

use std::sync::{Arc, Mutex};

use alloy_primitives::B256;

/// Signed transaction hashes whose broadcasts may have reached the node.
#[derive(Debug, Clone, Default)]
pub struct PublishedTransactions(Arc<Mutex<Vec<B256>>>);

impl PublishedTransactions {
    /// Returns every distinct hash attempted by this send, in publication order.
    pub fn hashes(&self) -> Vec<B256> {
        self.0.lock().expect("published transactions mutex poisoned").clone()
    }

    /// Records a signed hash before starting its broadcast.
    pub fn record(&self, hash: B256) {
        let mut hashes = self.0.lock().expect("published transactions mutex poisoned");
        if !hashes.contains(&hash) {
            hashes.push(hash);
        }
    }
}
