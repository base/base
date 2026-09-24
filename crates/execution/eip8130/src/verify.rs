//! Transaction-level secp256k1 actor authentication: resolve the sender and
//! payer of an [`Eip8130Signed`] to full-authority owners.

use alloy_primitives::Address;
use base_common_consensus::Eip8130Signed;

use crate::{ActorAuthorizer, RecoveredActorId, TxAuthError};

/// A resolved transaction actor: the account it authenticated as.
///
/// With the Keystore removed every actor is a full-authority owner, so an
/// authorized actor carries only its account address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthorizedActor {
    /// The account the actor authenticated as (the sender or payer account).
    pub account: Address,
}

/// The authorized actors of an EIP-8130 transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxActors {
    /// The transaction sender.
    pub sender: AuthorizedActor,
    /// The gas payer, or `None` when the sender implicitly pays (`tx.payer ==
    /// None`).
    pub payer: Option<AuthorizedActor>,
}

/// Authenticates the sender and payer of an [`Eip8130Signed`] as secp256k1
/// owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActorTxVerifier;

impl ActorTxVerifier {
    /// Resolves the transaction's sender and (optional) payer.
    ///
    /// Returns the authorized [`TxActors`], or the first [`TxAuthError`]
    /// encountered (sender checked before payer).
    pub fn verify(signed: &Eip8130Signed) -> Result<TxActors, TxAuthError> {
        Self::verify_with_recovered_sender(signed, None)
    }

    /// Like [`Self::verify`], but accepts an already-recovered EOA sender token
    /// to avoid a second secp256k1 recovery on the EOA path (`tx.sender ==
    /// None`). `recovered_sender` is ignored on the named path (`tx.sender ==
    /// Some`), and falls back to recovery here when `None`.
    pub fn verify_with_recovered_sender(
        signed: &Eip8130Signed,
        recovered_sender: Option<RecoveredActorId>,
    ) -> Result<TxActors, TxAuthError> {
        let tx = signed.tx();
        let sender = Self::verify_sender(signed, recovered_sender)?;

        let payer = match tx.payer {
            // Implicit self-pay: the sender covers its own gas.
            None => None,
            // Open mode: whoever signs the payer hash pays. The hash binds to the
            // resolved sender, so the signature cannot be reused for another
            // sender's identical body.
            Some(_) if tx.is_open_payer() => {
                let hash = tx.payer_signature_hash(sender.account);
                let account = ActorAuthorizer::recover_bare_k1(signed.payer_auth(), hash)
                    .map_err(|_| TxAuthError::PayerRecovery)?;
                Some(AuthorizedActor { account })
            }
            // Explicit self-pay (`payer == sender`) or a bound sponsor: a named
            // `K1_AUTHENTICATOR || sig` blob whose signer must be the named payer.
            Some(account) => {
                let hash = tx.payer_signature_hash(sender.account);
                let recovered = ActorAuthorizer::recover_named_k1(signed.payer_auth(), hash)?;
                if recovered != account {
                    return Err(TxAuthError::PayerMismatch { expected: account, recovered });
                }
                Some(AuthorizedActor { account })
            }
        };

        Ok(TxActors { sender, payer })
    }

    /// Resolves the sender for both the named path (`tx.sender == Some`) and the
    /// EOA path (`tx.sender == None`).
    fn verify_sender(
        signed: &Eip8130Signed,
        recovered_sender: Option<RecoveredActorId>,
    ) -> Result<AuthorizedActor, TxAuthError> {
        if let Some(account) = signed.explicit_sender() {
            // Named account: `sender_auth` is `K1_AUTHENTICATOR(20) || r||s||v`.
            let recovered = ActorAuthorizer::recover_named_k1(
                signed.sender_auth(),
                signed.tx().sender_signature_hash(),
            )?;
            if recovered != account {
                return Err(TxAuthError::SenderMismatch { expected: account, recovered });
            }
            return Ok(AuthorizedActor { account });
        }

        // EOA path: recover the sender exactly once with the checked (EIP-2
        // low-s) recovery. A caller that already recovered the sender passes the
        // token in so the ecrecover is not repeated.
        let recovered = match recovered_sender {
            Some(recovered) => recovered,
            None => RecoveredActorId::recover_eoa_sender(signed)
                .map_err(|_| TxAuthError::SenderRecovery)?
                .ok_or(TxAuthError::SenderRecovery)?,
        };
        Ok(AuthorizedActor { account: recovered.address() })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, U256, address, keccak256};
    use base_common_consensus::{Eip8130Constants, TxEip8130};
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;
    use crate::AuthError;

    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;

    fn key(byte: u8) -> K256SigningKey {
        K256SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    fn addr(key: &K256SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    fn sig(key: &K256SigningKey, hash: B256) -> Vec<u8> {
        let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = vec![0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    fn auth_blob(authenticator: Address, data: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(20 + data.len());
        out.extend_from_slice(authenticator.as_slice());
        out.extend_from_slice(data);
        Bytes::from(out)
    }

    fn base_tx(sender: Option<Address>, payer: Option<Address>) -> TxEip8130 {
        TxEip8130 {
            chain_id: 8453,
            sender,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer,
        }
    }

    #[test]
    fn eoa_sender_resolves_to_recovered_address() {
        let k = key(0x11);
        let account = addr(&k);
        let tx = base_tx(None, None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, Bytes::from(sig(&k, hash)), Bytes::new());
        let actors = ActorTxVerifier::verify(&signed).unwrap();
        assert_eq!(actors.sender.account, account);
        assert!(actors.payer.is_none());
    }

    #[test]
    fn named_sender_resolves_when_signer_matches() {
        let k = key(0x22);
        let account = addr(&k);
        let tx = base_tx(Some(account), None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&k, hash)), Bytes::new());
        let actors = ActorTxVerifier::verify(&signed).unwrap();
        assert_eq!(actors.sender.account, account);
    }

    #[test]
    fn named_sender_wrong_signer_is_rejected() {
        let k = key(0x22);
        let named = address!("0x00000000000000000000000000000000000000aa");
        let tx = base_tx(Some(named), None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&k, hash)), Bytes::new());
        assert!(matches!(
            ActorTxVerifier::verify(&signed),
            Err(TxAuthError::SenderMismatch { expected, .. }) if expected == named
        ));
    }

    #[test]
    fn named_sender_non_canonical_authenticator_is_rejected() {
        let k = key(0x22);
        let account = addr(&k);
        let bogus = address!("0x00000000000000000000000000000000deadbeef");
        let tx = base_tx(Some(account), None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(bogus, &sig(&k, hash)), Bytes::new());
        assert_eq!(
            ActorTxVerifier::verify(&signed),
            Err(TxAuthError::Authenticate(AuthError::NotCanonical(bogus)))
        );
    }

    #[test]
    fn sponsored_payer_resolves_against_payer_hash() {
        let sk = key(0x22);
        let sender_account = addr(&sk);
        let pk = key(0x33);
        let payer_account = addr(&pk);

        let tx = base_tx(Some(sender_account), Some(payer_account));
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            auth_blob(K1, &sig(&sk, sender_hash)),
            auth_blob(K1, &sig(&pk, payer_hash)),
        );
        let actors = ActorTxVerifier::verify(&signed).unwrap();
        assert_eq!(actors.payer.expect("payer present").account, payer_account);
    }

    #[test]
    fn eoa_sender_with_sponsored_payer_binds_recovered_address() {
        let sk = key(0x44);
        let sender_account = addr(&sk);
        let pk = key(0x55);
        let payer_account = addr(&pk);

        let tx = base_tx(None, Some(payer_account));
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sig(&sk, sender_hash)),
            auth_blob(K1, &sig(&pk, payer_hash)),
        );
        let actors = ActorTxVerifier::verify(&signed).unwrap();
        assert_eq!(actors.sender.account, sender_account);
        assert_eq!(actors.payer.expect("payer present").account, payer_account);
    }

    #[test]
    fn open_payer_resolves_to_the_recovered_signer() {
        let sk = key(0x46);
        let sender_account = addr(&sk);
        let pk = key(0x57);
        let payer_account = addr(&pk);

        let tx = base_tx(None, Some(Eip8130Constants::OPEN_PAYER));
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sig(&sk, sender_hash)),
            Bytes::from(sig(&pk, payer_hash)),
        );
        let actors = ActorTxVerifier::verify(&signed).unwrap();
        assert_eq!(actors.sender.account, sender_account);
        assert_eq!(actors.payer.expect("open payer resolves").account, payer_account);
    }

    #[test]
    fn open_payer_with_prefixed_auth_is_rejected() {
        let sk = key(0x48);
        let sender_account = addr(&sk);
        let pk = key(0x59);

        let tx = base_tx(None, Some(Eip8130Constants::OPEN_PAYER));
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sig(&sk, sender_hash)),
            auth_blob(K1, &sig(&pk, payer_hash)),
        );
        // A 20-byte-prefixed blob is not a valid bare 65-byte signature.
        assert_eq!(ActorTxVerifier::verify(&signed).unwrap_err(), TxAuthError::PayerRecovery);
    }

    #[test]
    fn named_payer_wrong_signer_is_rejected() {
        let sk = key(0x44);
        let pk = key(0x55);
        let payer_account = address!("0x00000000000000000000000000000000000000cc");

        let tx = base_tx(None, Some(payer_account));
        let sender_hash = tx.sender_signature_hash();
        // Payer signs the correct digest but is not the named payer account.
        let sender_account = addr(&sk);
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sig(&sk, sender_hash)),
            auth_blob(K1, &sig(&pk, payer_hash)),
        );
        assert!(matches!(
            ActorTxVerifier::verify(&signed),
            Err(TxAuthError::PayerMismatch { expected, .. }) if expected == payer_account
        ));
    }

    #[test]
    fn malformed_eoa_sender_signature_is_rejected() {
        let tx = base_tx(None, None);
        let signed = Eip8130Signed::new(tx, Bytes::from(vec![0u8; 64]), Bytes::new());
        assert_eq!(ActorTxVerifier::verify(&signed), Err(TxAuthError::SenderRecovery));
    }
}
