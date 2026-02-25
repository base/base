//! Traits for bundle operations.

use alloy_consensus::{Transaction, transaction::Recovered};
use alloy_primitives::{Address, B256, TxHash, keccak256};
use alloy_provider::network::eip2718::Encodable2718;
use base_alloy_consensus::OpTxEnvelope;
use base_alloy_flz::tx_estimated_size_fjord_bytes;

use crate::{AcceptedBundle, ParsedBundle};

/// Trait for types that contain bundle transactions.
pub trait BundleTxs {
    /// Returns a reference to the transactions in the bundle.
    fn transactions(&self) -> &Vec<Recovered<OpTxEnvelope>>;
}

/// Extension trait providing utility methods for bundle types.
pub trait BundleExtensions {
    /// Computes the bundle hash (keccak256 of concatenated transaction hashes).
    fn bundle_hash(&self) -> B256;
    /// Returns the transaction hashes in the bundle.
    fn txn_hashes(&self) -> Vec<TxHash>;
    /// Returns the sender addresses of all transactions.
    fn senders(&self) -> Vec<Address>;
    /// Returns the total gas limit of all transactions.
    fn gas_limit(&self) -> u64;
    /// Returns the estimated DA size in bytes (Fjord estimation).
    fn da_size(&self) -> u64;
}

impl<T: BundleTxs> BundleExtensions for T {
    fn bundle_hash(&self) -> B256 {
        let parsed = self.transactions();
        let mut concatenated = Vec::new();
        for tx in parsed {
            concatenated.extend_from_slice(tx.tx_hash().as_slice());
        }
        keccak256(&concatenated)
    }

    fn txn_hashes(&self) -> Vec<TxHash> {
        self.transactions().iter().map(|t| t.tx_hash()).collect()
    }

    fn senders(&self) -> Vec<Address> {
        self.transactions().iter().map(|t| t.signer()).collect()
    }

    fn gas_limit(&self) -> u64 {
        self.transactions().iter().map(|t| t.gas_limit()).sum()
    }

    fn da_size(&self) -> u64 {
        self.transactions().iter().map(|t| tx_estimated_size_fjord_bytes(&t.encoded_2718())).sum()
    }
}

impl BundleTxs for ParsedBundle {
    fn transactions(&self) -> &Vec<Recovered<OpTxEnvelope>> {
        &self.txs
    }
}

impl BundleTxs for AcceptedBundle {
    fn transactions(&self) -> &Vec<Recovered<OpTxEnvelope>> {
        &self.txs
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Keccak256, U256, keccak256};
    use alloy_provider::network::eip2718::Encodable2718;
    use alloy_signer_local::PrivateKeySigner;

    use super::ParsedBundle;
    use crate::{
        Bundle,
        test_utils::{create_test_meter_bundle_response, create_transaction},
    };

    #[test]
    fn test_bundle_hash_single_tx() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_bytes = tx.encoded_2718();

        let bundle =
            Bundle { txs: vec![tx_bytes.clone().into()], block_number: 1, ..Default::default() };

        let parsed: ParsedBundle = bundle.try_into().unwrap();

        let expected_hash = {
            let mut hasher = Keccak256::default();
            hasher.update(keccak256(&tx_bytes));
            hasher.finalize()
        };

        assert_eq!(parsed.bundle_hash(), expected_hash);
    }

    #[test]
    fn test_bundle_hash_multiple_txs() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx1 = create_transaction(alice.clone(), 1, bob.address(), U256::from(10_000));
        let tx2 = create_transaction(alice, 2, bob.address(), U256::from(20_000));
        let tx1_bytes = tx1.encoded_2718();
        let tx2_bytes = tx2.encoded_2718();

        let bundle = Bundle {
            txs: vec![tx1_bytes.clone().into(), tx2_bytes.clone().into()],
            block_number: 1,
            ..Default::default()
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();

        let expected_hash = {
            let mut hasher = Keccak256::default();
            hasher.update(keccak256(&tx1_bytes));
            hasher.update(keccak256(&tx2_bytes));
            hasher.finalize()
        };

        assert_eq!(parsed.bundle_hash(), expected_hash);
    }

    #[test]
    fn test_txn_hashes() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx1 = create_transaction(alice.clone(), 1, bob.address(), U256::from(10_000));
        let tx2 = create_transaction(alice, 2, bob.address(), U256::from(20_000));
        let tx1_hash = tx1.tx_hash();
        let tx2_hash = tx2.tx_hash();
        let tx1_bytes = tx1.encoded_2718();
        let tx2_bytes = tx2.encoded_2718();

        let bundle = Bundle {
            txs: vec![tx1_bytes.into(), tx2_bytes.into()],
            block_number: 1,
            ..Default::default()
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let hashes = parsed.txn_hashes();

        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes[0], tx1_hash);
        assert_eq!(hashes[1], tx2_hash);
    }

    #[test]
    fn test_senders() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice.clone(), 1, bob.address(), U256::from(10_000));
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle { txs: vec![tx_bytes.into()], block_number: 1, ..Default::default() };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let senders = parsed.senders();

        assert_eq!(senders.len(), 1);
        assert_eq!(senders[0], alice.address());
    }

    #[test]
    fn test_gas_limit() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx1 = create_transaction(alice.clone(), 1, bob.address(), U256::from(10_000));
        let tx2 = create_transaction(alice, 2, bob.address(), U256::from(20_000));
        let tx1_bytes = tx1.encoded_2718();
        let tx2_bytes = tx2.encoded_2718();

        let bundle = Bundle {
            txs: vec![tx1_bytes.into(), tx2_bytes.into()],
            block_number: 1,
            ..Default::default()
        };

        let parsed: ParsedBundle = bundle.try_into().unwrap();

        // Each transaction has gas limit of 21000
        assert_eq!(parsed.gas_limit(), 42000);
    }

    #[test]
    fn test_da_size() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle { txs: vec![tx_bytes.into()], block_number: 1, ..Default::default() };

        let parsed: ParsedBundle = bundle.try_into().unwrap();

        // DA size should be > 0 for any transaction
        assert!(parsed.da_size() > 0);
    }

    #[test]
    fn test_accepted_bundle_traits() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx = create_transaction(alice, 1, bob.address(), U256::from(10_000));
        let tx_hash = tx.tx_hash();
        let tx_bytes = tx.encoded_2718();

        let bundle = Bundle { txs: vec![tx_bytes.into()], block_number: 1, ..Default::default() };

        let parsed: ParsedBundle = bundle.try_into().unwrap();
        let accepted = AcceptedBundle::new(parsed.clone(), create_test_meter_bundle_response());

        // Both should have the same bundle hash
        assert_eq!(parsed.bundle_hash(), accepted.bundle_hash());
        assert_eq!(parsed.txn_hashes(), accepted.txn_hashes());
        assert_eq!(parsed.senders(), accepted.senders());
        assert_eq!(parsed.gas_limit(), accepted.gas_limit());
        assert_eq!(parsed.da_size(), accepted.da_size());
        assert_eq!(accepted.txn_hashes()[0], tx_hash);
    }
}
