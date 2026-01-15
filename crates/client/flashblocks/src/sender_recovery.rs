//! Parallel sender recovery service.
//!
//! This module provides [`SenderRecoveryService`] for efficient parallel ECDSA
//! signature recovery with caching support.

use std::collections::HashMap;

use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::{Address, B256};
use op_alloy_consensus::OpTxEnvelope;
use rayon::prelude::*;

use crate::Result;

/// Service for parallel sender recovery with caching.
///
/// Performs ECDSA signature recovery in parallel across all transactions,
/// utilizing cached sender addresses when available to avoid redundant
/// cryptographic operations.
#[derive(Debug, Default)]
pub struct SenderRecoveryService;

impl SenderRecoveryService {
    /// Creates a new sender recovery service.
    pub const fn new() -> Self {
        Self
    }

    /// Recovers senders for all transactions in parallel.
    ///
    /// Uses cached senders when available, falling back to ECDSA recovery
    /// for uncached transactions. All recovery operations run in parallel
    /// using rayon.
    ///
    /// # Arguments
    /// * `transactions` - Iterator of transactions to recover senders for.
    /// * `cached_senders` - Map of transaction hashes to known sender addresses.
    ///
    /// # Returns
    /// A vector of (transaction, sender) tuples in the same order as input.
    ///
    /// # Errors
    /// Returns an error if any signature recovery fails.
    pub fn recover_all<'a>(
        transactions: impl IntoIterator<Item = &'a OpTxEnvelope>,
        cached_senders: &HashMap<B256, Address>,
    ) -> Result<Vec<(OpTxEnvelope, Address)>> {
        let txs: Vec<_> = transactions.into_iter().collect();

        txs.par_iter()
            .map(|tx| {
                let tx_hash = tx.tx_hash();
                let sender = match cached_senders.get(&tx_hash) {
                    Some(&cached) => cached,
                    None => tx.recover_signer()?,
                };
                Ok(((*tx).clone(), sender))
            })
            .collect()
    }

    /// Recovers senders for transactions, using a lookup function for caching.
    ///
    /// This variant allows for more flexible cache implementations beyond
    /// a simple HashMap.
    ///
    /// # Arguments
    /// * `transactions` - Iterator of transactions to recover senders for.
    /// * `cache_lookup` - Function that returns cached sender for a tx hash, if available.
    ///
    /// # Returns
    /// A vector of (transaction, sender) tuples in the same order as input.
    ///
    /// # Errors
    /// Returns an error if any signature recovery fails.
    pub fn recover_all_with_lookup<'a, F>(
        transactions: impl IntoIterator<Item = &'a OpTxEnvelope>,
        cache_lookup: F,
    ) -> Result<Vec<(OpTxEnvelope, Address)>>
    where
        F: Fn(&B256) -> Option<Address> + Sync,
    {
        let txs: Vec<_> = transactions.into_iter().collect();

        txs.par_iter()
            .map(|tx| {
                let tx_hash = tx.tx_hash();
                let sender = match cache_lookup(&tx_hash) {
                    Some(cached) => cached,
                    None => tx.recover_signer()?,
                };
                Ok(((*tx).clone(), sender))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::SignableTransaction;
    use alloy_primitives::{TxKind, U256};
    use op_alloy_consensus::OpTxEnvelope;

    use super::*;

    fn create_signed_tx(signer: &alloy_signer_local::PrivateKeySigner) -> OpTxEnvelope {
        use alloy_consensus::TxEip1559;
        use alloy_signer::SignerSync;

        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 1000000000,
            max_priority_fee_per_gas: 1000000000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
            access_list: Default::default(),
        };

        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        OpTxEnvelope::Eip1559(tx.into_signed(signature))
    }

    #[test]
    fn test_recover_all_empty() {
        let txs: Vec<&OpTxEnvelope> = vec![];
        let cached = HashMap::new();

        let result = SenderRecoveryService::recover_all(txs, &cached);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_recover_all_with_cache_hit() {
        let signer = alloy_signer_local::PrivateKeySigner::random();
        let tx = create_signed_tx(&signer);
        let tx_hash = tx.tx_hash();
        let expected_sender = signer.address();

        let mut cached = HashMap::new();
        cached.insert(tx_hash, expected_sender);

        let result = SenderRecoveryService::recover_all([&tx], &cached);
        assert!(result.is_ok());

        let recovered = result.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].1, expected_sender);
    }

    #[test]
    fn test_recover_all_with_cache_miss() {
        let signer = alloy_signer_local::PrivateKeySigner::random();
        let tx = create_signed_tx(&signer);
        let expected_sender = signer.address();

        let cached = HashMap::new(); // Empty cache

        let result = SenderRecoveryService::recover_all([&tx], &cached);
        assert!(result.is_ok());

        let recovered = result.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].1, expected_sender);
    }

    #[test]
    fn test_recover_all_with_lookup() {
        let signer = alloy_signer_local::PrivateKeySigner::random();
        let tx = create_signed_tx(&signer);
        let tx_hash = tx.tx_hash();
        let expected_sender = signer.address();

        let lookup = |hash: &B256| {
            if *hash == tx_hash {
                Some(expected_sender)
            } else {
                None
            }
        };

        let result = SenderRecoveryService::recover_all_with_lookup([&tx], lookup);
        assert!(result.is_ok());

        let recovered = result.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].1, expected_sender);
    }
}
