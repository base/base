use std::{str::FromStr, sync::Arc};

use alloy_consensus::SignableTransaction;
use alloy_primitives::{Address, B256, Signature};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use k256::sha2::{Digest, Sha256};
use op_alloy_consensus::OpTypedTransaction;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::Recovered;

/// Simple struct to sign txs/messages.
/// Mainly used to sign payout txs from the builder and to create test data.
///
/// This is a wrapper around [`PrivateKeySigner`] that provides convenient
/// methods for signing OP Stack transactions.
#[derive(Debug, Clone)]
pub struct Signer {
    inner: PrivateKeySigner,
    /// The cached address of the signer for quick access.
    pub address: Address,
}

impl PartialEq for Signer {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

impl Eq for Signer {}

impl Signer {
    /// Creates a new signer from a secret key.
    pub fn try_from_secret(secret: B256) -> eyre::Result<Self> {
        let inner = PrivateKeySigner::from_bytes(&secret)?;
        let address = inner.address();
        Ok(Self { inner, address })
    }

    /// Signs a message hash and returns the signature.
    pub fn sign_message(&self, message: B256) -> eyre::Result<Signature> {
        self.inner.sign_hash_sync(&message).map_err(|e| eyre::eyre!("failed to sign message: {e}"))
    }

    /// Signs a transaction and returns the recovered signed transaction.
    pub fn sign_tx(&self, tx: OpTypedTransaction) -> eyre::Result<Recovered<OpTransactionSigned>> {
        let signature_hash = match &tx {
            OpTypedTransaction::Legacy(tx) => tx.signature_hash(),
            OpTypedTransaction::Eip2930(tx) => tx.signature_hash(),
            OpTypedTransaction::Eip1559(tx) => tx.signature_hash(),
            OpTypedTransaction::Eip7702(tx) => tx.signature_hash(),
            OpTypedTransaction::Deposit(_) => B256::ZERO,
        };
        let signature = self.sign_message(signature_hash)?;
        let signed = OpTransactionSigned::new_unhashed(tx, signature);
        Ok(Recovered::new_unchecked(signed, self.address))
    }

    /// Creates a random signer.
    pub fn random() -> Self {
        let inner = PrivateKeySigner::random();
        let address = inner.address();
        Self { inner, address }
    }

    /// Returns the secret key bytes.
    ///
    /// # Warning
    /// This exposes the private key material. Use with caution.
    pub fn secret_bytes(&self) -> B256 {
        B256::from_slice(&self.inner.credential().to_bytes())
    }
}

impl FromStr for Signer {
    type Err = eyre::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from_secret(B256::from_str(s)?)
    }
}

/// Thread-safe reference to a [`Signer`].
///
/// Use this type when sharing a signer across multiple components.
/// Cloning is cheap (just an atomic reference count increment).
pub type SignerRef = Arc<Signer>;

/// Generates a new random signer.
pub fn generate_signer() -> Signer {
    Signer::random()
}

/// Generates a key deterministically from a seed for debug and testing.
///
/// # Warning
/// Do not use in production.
pub fn generate_key_from_seed(seed: &str) -> Signer {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let hash = hasher.finalize();

    Signer::try_from_secret(B256::from_slice(&hash)).expect("Failed to create signer from seed")
}

#[cfg(test)]
mod test {
    use alloy_consensus::{TxEip1559, transaction::SignerRecoverable};
    use alloy_primitives::{TxKind as TransactionKind, U256, address, fixed_bytes};

    use super::*;

    #[test]
    fn test_sign_transaction() {
        let secret =
            fixed_bytes!("7a3233fcd52c19f9ffce062fd620a8888930b086fba48cfea8fc14aac98a4dce");
        let address = address!("B2B9609c200CA9b7708c2a130b911dabf8B49B20");
        let signer = Signer::try_from_secret(secret).expect("signer creation");
        assert_eq!(signer.address, address);

        let tx = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 2,
            gas_limit: 21000,
            max_fee_per_gas: 1000,
            max_priority_fee_per_gas: 20000,
            to: TransactionKind::Call(address),
            value: U256::from(3000u128),
            ..Default::default()
        });

        let signed_tx = signer.sign_tx(tx).expect("sign tx");
        assert_eq!(signed_tx.signer(), address);

        let signed = signed_tx.into_inner();
        assert_eq!(signed.recover_signer().ok(), Some(address));
    }

    #[test]
    fn test_random_signer() {
        let signer1 = Signer::random();
        let signer2 = Signer::random();
        assert_ne!(signer1.address, signer2.address);
    }

    #[test]
    fn test_deterministic_seed() {
        let signer1 = generate_key_from_seed("test_seed");
        let signer2 = generate_key_from_seed("test_seed");
        assert_eq!(signer1.address, signer2.address);

        let signer3 = generate_key_from_seed("different_seed");
        assert_ne!(signer1.address, signer3.address);
    }

    #[test]
    fn test_secret_bytes_roundtrip() {
        let signer = Signer::random();
        let secret = signer.secret_bytes();
        let recovered = Signer::try_from_secret(secret).expect("should recover");
        assert_eq!(signer.address, recovered.address);
    }
}
