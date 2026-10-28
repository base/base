//! Checks on the sender-signed transaction a `payer_sendTransaction` request carries.

use alloy_primitives::{Address, U256};
use base_common_consensus::{
    BaseTxEnvelope, Eip8130Constants, Eip8130Signed, decode_2718_canonical,
};
use thiserror::Error;

/// An EIP-8130 transaction that names a payer, leaves `payer_auth` for it to fill, and carries
/// a sender signature that recovers.
#[derive(Debug, Clone)]
pub struct PayerTransaction {
    signed: Eip8130Signed,
    sender: Address,
}

impl PayerTransaction {
    /// Decodes `raw` and checks it against `chain_id`.
    ///
    /// A bare `sender_auth` must recover the sender. A declared sender must sign with
    /// [`Eip8130Constants::K1_AUTHENTICATOR`] using its own key unless `keystore` is active, in
    /// which case it may authorize other keys and other authenticators. Whether it did needs
    /// account state, so with the keystore only a K1 signature is checked, and only that it
    /// recovers.
    pub fn decode(
        raw: &[u8],
        chain_id: u64,
        keystore: bool,
    ) -> Result<Self, PayerTransactionError> {
        let envelope = decode_2718_canonical::<BaseTxEnvelope>(raw)
            .map_err(|_| PayerTransactionError::Undecodable)?;
        let BaseTxEnvelope::Eip8130(signed) = envelope else {
            return Err(PayerTransactionError::NotEip8130);
        };
        if signed.tx().chain_id != chain_id {
            return Err(PayerTransactionError::ChainIdMismatch);
        }
        if signed.tx().payer.is_none() {
            return Err(PayerTransactionError::MissingPayer);
        }
        if !signed.payer_auth().is_empty() {
            return Err(PayerTransactionError::PayerAuthPresent);
        }
        let Some(sender) = signed.explicit_sender() else {
            let sender =
                signed.recover_sender().map_err(|_| PayerTransactionError::SenderSignature)?;
            return Ok(Self { signed, sender });
        };
        let k1_signature =
            signed.sender_auth().strip_prefix(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        let Some(signature) = k1_signature else {
            if !keystore {
                return Err(PayerTransactionError::UnsupportedSenderAuthenticator);
            }
            return Ok(Self { signed, sender });
        };
        let signer = Eip8130Signed::recover_raw_k1(signed.tx().sender_signature_hash(), signature)
            .map_err(|_| PayerTransactionError::SenderSignature)?;
        if signer != sender && !keystore {
            return Err(PayerTransactionError::SenderMismatch);
        }
        Ok(Self { signed, sender })
    }

    /// The sender and `nonce_key` whose nonce this transaction uses, or `None` for a nonce-free
    /// transaction, which relies on its expiry instead.
    pub fn nonce_channel(&self) -> Option<(Address, U256)> {
        let nonce_key = self.signed.tx().nonce_key;
        (nonce_key != Eip8130Constants::NONCE_KEY_MAX).then_some((self.sender, nonce_key))
    }

    /// Rejects this transaction when `next`, the next unused nonce in its channel, is past its
    /// `nonce_sequence`. Such a transaction can never be included, such as one replayed from
    /// chain with its `payer_auth` stripped.
    pub fn check_nonce(&self, next: U256) -> Result<(), PayerTransactionError> {
        if U256::from(self.signed.tx().nonce_sequence) < next {
            return Err(PayerTransactionError::NonceUsed);
        }
        Ok(())
    }
}

/// A `payer_sendTransaction` transaction rejected before it is forwarded.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PayerTransactionError {
    /// The bytes are not a canonical EIP-2718 transaction.
    #[error("transaction is not a valid EIP-2718 encoding")]
    Undecodable,
    /// The transaction is not EIP-8130.
    #[error("payer_sendTransaction requires an EIP-8130 transaction")]
    NotEip8130,
    /// The transaction's chain id is not this node's.
    #[error("transaction chain id does not match this node")]
    ChainIdMismatch,
    /// The transaction does not set `payer`.
    #[error("transaction must name the offer's payer")]
    MissingPayer,
    /// The transaction already carries `payer_auth`.
    #[error("payer_auth must be empty for the payer to fill")]
    PayerAuthPresent,
    /// `sender_auth` does not recover a signer.
    #[error("sender signature does not recover")]
    SenderSignature,
    /// A declared sender's K1 signature recovers to another address.
    #[error("sender signature is not from the declared sender")]
    SenderMismatch,
    /// A declared sender signs with an authenticator other than K1 before the keystore upgrade.
    #[error("a declared sender must sign with the K1 authenticator")]
    UnsupportedSenderAuthenticator,
    /// The transaction's nonce was already used.
    #[error("transaction nonce was already used")]
    NonceUsed,
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Bytes, Signature};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::{Call, TxEip8130};

    use super::*;

    const CHAIN_ID: u64 = 8453;

    fn call(to: u8) -> Call {
        Call { to: Address::repeat_byte(to), value: U256::ZERO, data: Bytes::new() }
    }

    fn transaction(sender: Option<Address>, payer: Option<Address>) -> TxEip8130 {
        TxEip8130 {
            chain_id: CHAIN_ID,
            sender,
            max_fee_per_gas: 1,
            gas_limit: 100_000,
            calls: vec![vec![call(0x11)], vec![call(0x22)]],
            payer,
            ..Default::default()
        }
    }

    fn encode(tx: TxEip8130, sender_auth: Vec<u8>, payer_auth: Bytes) -> Vec<u8> {
        BaseTxEnvelope::Eip8130(Eip8130Signed::new(tx, sender_auth.into(), payer_auth))
            .encoded_2718()
    }

    fn signature(signer: &PrivateKeySigner, tx: &TxEip8130) -> [u8; 65] {
        signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes()
    }

    fn k1_auth(signer: &PrivateKeySigner, tx: &TxEip8130) -> Vec<u8> {
        [Eip8130Constants::K1_AUTHENTICATOR.as_slice(), &signature(signer, tx)].concat()
    }

    fn payer() -> Option<Address> {
        Some(Address::repeat_byte(0xaa))
    }

    #[test]
    fn accepts_an_eoa_transaction_from_the_key_that_signed_it() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(None, payer());
        let raw = encode(tx.clone(), signature(&signer, &tx).to_vec(), Bytes::new());

        let transaction = PayerTransaction::decode(&raw, CHAIN_ID, false).unwrap();

        assert_eq!(transaction.nonce_channel(), Some((signer.address(), U256::ZERO)));
    }

    #[test]
    fn accepts_an_open_payer() {
        let signer = PrivateKeySigner::random();
        let tx = transaction(None, Some(Eip8130Constants::OPEN_PAYER));
        let raw = encode(tx.clone(), signature(&signer, &tx).to_vec(), Bytes::new());

        assert!(PayerTransaction::decode(&raw, CHAIN_ID, false).is_ok());
    }

    #[test]
    fn rejects_a_transaction_the_payer_cannot_co_sign() {
        let signer = PrivateKeySigner::random();
        let signed = |tx: TxEip8130, payer_auth: Bytes| {
            encode(tx.clone(), signature(&signer, &tx).to_vec(), payer_auth)
        };

        let self_paid = signed(transaction(None, None), Bytes::new());
        let co_signed = signed(transaction(None, payer()), Bytes::from(vec![0x01; 85]));
        let mut other_chain = transaction(None, payer());
        other_chain.chain_id = 84532;
        let other_chain = signed(other_chain, Bytes::new());

        assert_eq!(
            PayerTransaction::decode(&self_paid, CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::MissingPayer
        );
        assert_eq!(
            PayerTransaction::decode(&co_signed, CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::PayerAuthPresent
        );
        assert_eq!(
            PayerTransaction::decode(&other_chain, CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::ChainIdMismatch
        );
    }

    #[test]
    fn rejects_a_nonce_its_channel_already_used() {
        let signer = PrivateKeySigner::random();
        let mut tx = transaction(None, payer());
        tx.nonce_key = U256::from(7);
        tx.nonce_sequence = 3;
        let raw = encode(tx.clone(), signature(&signer, &tx).to_vec(), Bytes::new());
        let transaction = PayerTransaction::decode(&raw, CHAIN_ID, false).unwrap();

        assert_eq!(transaction.nonce_channel(), Some((signer.address(), U256::from(7))));
        assert_eq!(transaction.check_nonce(U256::from(4)), Err(PayerTransactionError::NonceUsed));
        assert_eq!(transaction.check_nonce(U256::from(3)), Ok(()));
        assert_eq!(transaction.check_nonce(U256::from(2)), Ok(()));
    }

    #[test]
    fn leaves_a_nonce_free_transaction_to_its_expiry() {
        let signer = PrivateKeySigner::random();
        let mut tx = transaction(None, payer());
        tx.nonce_key = Eip8130Constants::NONCE_KEY_MAX;
        tx.valid_before = 1;
        let raw = encode(tx.clone(), signature(&signer, &tx).to_vec(), Bytes::new());

        assert_eq!(PayerTransaction::decode(&raw, CHAIN_ID, false).unwrap().nonce_channel(), None);
    }

    #[test]
    fn rejects_bytes_that_are_not_an_eip8130_transaction() {
        let signed = Signed::new_unhashed(TxEip1559::default(), Signature::test_signature());
        let eip1559 = BaseTxEnvelope::Eip1559(signed).encoded_2718();

        assert_eq!(
            PayerTransaction::decode(&eip1559, CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::NotEip8130
        );
        assert_eq!(
            PayerTransaction::decode(&[0x79, 0xff], CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::Undecodable
        );
    }

    #[test]
    fn rejects_an_eoa_signature_that_does_not_recover() {
        let raw = encode(transaction(None, payer()), vec![0x01; 65], Bytes::new());

        assert_eq!(
            PayerTransaction::decode(&raw, CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::SenderSignature
        );
    }

    #[test]
    fn requires_a_declared_sender_to_sign_with_its_own_k1_key_before_the_keystore() {
        let sender = PrivateKeySigner::random();
        let other = PrivateKeySigner::random();
        let tx = transaction(Some(sender.address()), payer());
        let own_key = encode(tx.clone(), k1_auth(&sender, &tx), Bytes::new());
        let other_key = encode(tx.clone(), k1_auth(&other, &tx), Bytes::new());
        let mut passkey_auth = Address::repeat_byte(0x05).to_vec();
        passkey_auth.extend_from_slice(&[0x01; 64]);
        let passkey = encode(tx, passkey_auth, Bytes::new());

        let own_key = PayerTransaction::decode(&own_key, CHAIN_ID, false).unwrap();
        assert_eq!(own_key.nonce_channel(), Some((sender.address(), U256::ZERO)));
        assert_eq!(
            PayerTransaction::decode(&other_key, CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::SenderMismatch
        );
        assert_eq!(
            PayerTransaction::decode(&passkey, CHAIN_ID, false).unwrap_err(),
            PayerTransactionError::UnsupportedSenderAuthenticator
        );
    }

    #[test]
    fn lets_a_declared_sender_authorize_other_keys_with_the_keystore() {
        let sender = PrivateKeySigner::random();
        let session_key = PrivateKeySigner::random();
        let tx = transaction(Some(sender.address()), payer());
        let session = encode(tx.clone(), k1_auth(&session_key, &tx), Bytes::new());
        let mut passkey_auth = Address::repeat_byte(0x05).to_vec();
        passkey_auth.extend_from_slice(&[0x01; 64]);
        let passkey = encode(tx.clone(), passkey_auth, Bytes::new());
        let mut forged = k1_auth(&sender, &tx);
        forged[84] = 0x00;
        let forged = encode(tx, forged, Bytes::new());

        let session = PayerTransaction::decode(&session, CHAIN_ID, true).unwrap();
        assert_eq!(session.nonce_channel(), Some((sender.address(), U256::ZERO)));
        assert!(PayerTransaction::decode(&passkey, CHAIN_ID, true).is_ok());
        assert_eq!(
            PayerTransaction::decode(&forged, CHAIN_ID, true).unwrap_err(),
            PayerTransactionError::SenderSignature
        );
    }
}
