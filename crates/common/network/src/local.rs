use std::fmt;

use alloy_primitives::{Address, B256, ChainId, Signature};
use alloy_signer::{Result, Signer, SignerSync, sign_transaction_with_chain_id};
use async_trait::async_trait;
use base_common_consensus::SignableTransaction;
use k256::ecdsa::SigningKey;

use crate::{TxSigner, TxSignerSync, impl_into_wallet};

/// A transaction signer backed by a local secp256k1 private key.
#[derive(Clone)]
pub struct PrivateKeySigner {
    /// The signer's credential.
    pub credential: SigningKey,
    /// The signer's address.
    pub address: Address,
    /// The signer's chain ID (for EIP-155).
    pub chain_id: Option<ChainId>,
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl Signer for PrivateKeySigner {
    #[inline]
    async fn sign_hash(&self, hash: &B256) -> Result<Signature> {
        self.sign_hash_sync(hash)
    }

    #[inline]
    fn address(&self) -> Address {
        self.address
    }

    #[inline]
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id
    }

    #[inline]
    fn set_chain_id(&mut self, chain_id: Option<ChainId>) {
        self.chain_id = chain_id;
    }
}

impl SignerSync for PrivateKeySigner {
    #[inline]
    fn sign_hash_sync(&self, hash: &B256) -> Result<Signature> {
        Ok(self.credential.sign_prehash_recoverable(hash.as_ref())?.into())
    }

    #[inline]
    fn chain_id_sync(&self) -> Option<ChainId> {
        self.chain_id
    }
}

impl PrivateKeySigner {
    /// Constructs a signer from a signing key and its address.
    ///
    /// `address` is trusted and is not derived from or checked against `credential`. The caller
    /// must ensure it is the address recovered from signatures produced by the credential.
    /// `chain_id` affects transaction signing only.
    #[inline]
    pub const fn new_with_credential(
        credential: SigningKey,
        address: Address,
        chain_id: Option<ChainId>,
    ) -> Self {
        Self { credential, address, chain_id }
    }

    /// Returns this signer's credential.
    ///
    /// The returned value exposes private-key material. Do not log or
    /// otherwise disclose it.
    #[inline]
    pub const fn credential(&self) -> &SigningKey {
        &self.credential
    }

    /// Consumes this signer and returns its credential.
    ///
    /// The returned value exposes private-key material. Do not log or
    /// otherwise disclose it.
    #[inline]
    pub fn into_credential(self) -> SigningKey {
        self.credential
    }

    /// Returns this signer's address.
    #[inline]
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Returns this signer's chain ID.
    #[inline]
    pub const fn chain_id(&self) -> Option<ChainId> {
        self.chain_id
    }
}

// do not log the signer
impl fmt::Debug for PrivateKeySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateKeySigner")
            .field("address", &self.address)
            .field("chain_id", &self.chain_id)
            .finish()
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl TxSigner<Signature> for PrivateKeySigner {
    fn address(&self) -> Address {
        self.address
    }

    #[doc(alias = "sign_tx")]
    async fn sign_transaction(
        &self,
        tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy_signer::Result<Signature> {
        sign_transaction_with_chain_id!(self, tx, self.sign_hash_sync(&tx.signature_hash()))
    }
}

impl TxSignerSync<Signature> for PrivateKeySigner {
    fn address(&self) -> Address {
        self.address
    }

    #[doc(alias = "sign_tx_sync")]
    fn sign_transaction_sync(
        &self,
        tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy_signer::Result<Signature> {
        sign_transaction_with_chain_id!(self, tx, self.sign_hash_sync(&tx.signature_hash()))
    }
}

impl_into_wallet!(PrivateKeySigner);

#[cfg(test)]
mod test {
    use alloy_primitives::{U256, address};
    use base_common_consensus::TxLegacy;

    use super::*;

    #[tokio::test]
    async fn signs_tx() {
        async fn sign_tx_test(tx: &mut TxLegacy, chain_id: Option<ChainId>) -> Result<Signature> {
            let mut before = tx.clone();
            let sig = sign_dyn_tx_test(tx, chain_id).await?;
            if let Some(chain_id) = chain_id {
                assert_eq!(tx.chain_id, Some(chain_id), "chain ID was not set");
                before.chain_id = Some(chain_id);
            }
            assert_eq!(*tx, before);
            Ok(sig)
        }

        async fn sign_dyn_tx_test(
            tx: &mut dyn SignableTransaction<Signature>,
            chain_id: Option<ChainId>,
        ) -> Result<Signature> {
            let mut signer: PrivateKeySigner =
                "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318".parse().unwrap();
            signer.set_chain_id(chain_id);

            let sig = signer.sign_transaction_sync(tx)?;
            let sighash = tx.signature_hash();
            assert_eq!(sig.recover_address_from_prehash(&sighash).unwrap(), signer.address());

            let sig_async = signer.sign_transaction(tx).await.unwrap();
            assert_eq!(sig_async, sig);

            Ok(sig)
        }

        // retrieved test vector from:
        // https://web3js.readthedocs.io/en/v1.2.0/web3-eth-accounts.html#eth-accounts-signtransaction
        let mut tx = TxLegacy {
            to: address!("F0109fC8DF283027b6285cc889F5aA624EaC1F55").into(),
            value: U256::from(1_000_000_000),
            gas_limit: 2_000_000,
            nonce: 0,
            gas_price: 21_000_000_000,
            input: Default::default(),
            chain_id: None,
        };
        let sig_none = sign_tx_test(&mut tx, None).await.unwrap();

        tx.chain_id = Some(1);
        let sig_1 = sign_tx_test(&mut tx, None).await.unwrap();
        let expected = "c9cf86333bcb065d140032ecaab5d9281bde80f21b9687b3e94161de42d51895727a108a0b8d101465414033c3f705a9c7b826e596766046ee1183dbc8aeaa6825".parse().unwrap();
        assert_eq!(sig_1, expected);
        assert_ne!(sig_1, sig_none);

        tx.chain_id = Some(2);
        let sig_2 = sign_tx_test(&mut tx, None).await.unwrap();
        assert_ne!(sig_2, sig_1);
        assert_ne!(sig_2, sig_none);

        // Sets chain ID.
        tx.chain_id = None;
        let sig_none_none = sign_tx_test(&mut tx, None).await.unwrap();
        assert_eq!(sig_none_none, sig_none);

        tx.chain_id = None;
        let sig_none_1 = sign_tx_test(&mut tx, Some(1)).await.unwrap();
        assert_eq!(sig_none_1, sig_1);

        tx.chain_id = None;
        let sig_none_2 = sign_tx_test(&mut tx, Some(2)).await.unwrap();
        assert_eq!(sig_none_2, sig_2);

        // Errors on mismatch.
        tx.chain_id = Some(2);
        let error = sign_tx_test(&mut tx, Some(1)).await.unwrap_err();
        let expected_error = alloy_signer::Error::TransactionChainIdMismatch { signer: 1, tx: 2 };
        assert_eq!(error.to_string(), expected_error.to_string());
    }

    // <https://github.com/alloy-rs/core/issues/705>
    #[test]
    fn test_parity() {
        let signer = PrivateKeySigner::random();
        let message = b"hello";
        let signature = signer.sign_message_sync(message).unwrap();
        let value = signature.as_bytes().to_vec();
        let recovered_signature: Signature = value.as_slice().try_into().unwrap();
        assert_eq!(signature, recovered_signature);
    }
}
