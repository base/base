//! EIP-8130 transaction authorization: resolves the sender and payer by native
//! secp256k1 recovery and records the transaction's delegation entry.

use alloy_primitives::{Address, B256};
use base_common_consensus::{AccountChange, Delegation, Eip8130Constants, Eip8130Signed};

use crate::{DelegationEffect, TxAuthError};

/// The authorized result of an EIP-8130 transaction: its sender, its payer, and
/// the delegation the execution layer must install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthorizedTransaction {
    /// The authenticated sender.
    pub sender: Address,
    /// The authenticated gas payer: the sender for self-pay, the named payer, or
    /// the signer of an open-mode `payer_auth`.
    pub payer: Address,
    /// The delegation set or cleared by the transaction's delegation entry, if
    /// any. Not yet installed.
    pub delegation: Option<DelegationEffect>,
}

/// The sender account of a signed EIP-8130 transaction, resolved but not yet
/// authenticated: the explicit wire `sender` when it is named, or the recovered
/// signer of a bare `sender_auth`.
///
/// Resolving the sender ahead of authorization lets block execution check the
/// nonce first. A recovered sender is already authenticated, so
/// [`TransactionAuthorizer`] does not run the secp256k1 recovery twice, and the
/// transaction hash binds it to the `signed` it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSender {
    account: Address,
    recovered: bool,
    tx_hash: B256,
}

impl ResolvedSender {
    /// Resolves the sender account of `signed`.
    ///
    /// # Errors
    /// Returns [`TxAuthError::SenderRecovery`] when a bare `sender_auth` does
    /// not recover a signer.
    pub fn resolve(signed: &Eip8130Signed) -> Result<Self, TxAuthError> {
        let tx_hash = *signed.hash();
        if let Some(account) = signed.explicit_sender() {
            return Ok(Self { account, recovered: false, tx_hash });
        }
        let account = signed
            .recover_eoa_sender()
            .map_err(|_| TxAuthError::SenderRecovery)?
            .ok_or(TxAuthError::SenderRecovery)?;
        Ok(Self { account, recovered: true, tx_hash })
    }

    /// The resolved sender account.
    pub const fn account(&self) -> Address {
        self.account
    }

    /// Checks that this sender was resolved from `signed`.
    ///
    /// # Errors
    /// Returns [`TxAuthError::ResolvedSenderMismatch`] when it was resolved
    /// from a different transaction.
    pub fn check_resolved_from(&self, signed: &Eip8130Signed) -> Result<(), TxAuthError> {
        if self.tx_hash == *signed.hash() {
            Ok(())
        } else {
            Err(TxAuthError::ResolvedSenderMismatch)
        }
    }
}

/// Authorizes signed EIP-8130 transactions.
///
/// Authorization never reads state. Every sender and payer is authenticated by
/// native secp256k1 recovery, and the recovered key is the account's own key,
/// which authorizes everything the transaction type allows: originating the
/// transaction, paying for it, and delegating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransactionAuthorizer;

impl TransactionAuthorizer {
    /// Authorizes `signed`, whose sender was resolved as `sender`.
    ///
    /// - [`Self::check`] runs first;
    /// - a bare `sender_auth` recovers the sender; a named sender needs
    ///   `K1_AUTHENTICATOR || sig` recovering to that address;
    /// - an open payer is whoever signed `payer_auth`; a named payer needs
    ///   `K1_AUTHENTICATOR || sig` recovering to that address;
    /// - the delegation entry, if any, is recorded by [`Self::delegation`].
    ///
    /// # Errors
    /// Returns the first [`TxAuthError`] encountered, including
    /// [`TxAuthError::ResolvedSenderMismatch`] for a `sender` resolved from a
    /// different transaction.
    pub fn authorize(
        signed: &Eip8130Signed,
        sender: ResolvedSender,
    ) -> Result<AuthorizedTransaction, TxAuthError> {
        sender.check_resolved_from(signed)?;
        Self::check(signed)?;
        let tx = signed.tx();

        let sender = if sender.recovered {
            sender.account
        } else {
            Self::recover_named_k1(
                sender.account,
                tx.sender_signature_hash(),
                signed.sender_auth(),
            )?
        };

        let payer = match tx.payer {
            None => sender,
            Some(_) if tx.is_open_payer() => {
                Eip8130Signed::recover_raw_k1(tx.payer_signature_hash(sender), signed.payer_auth())
                    .map_err(|_| TxAuthError::PayerRecovery)?
            }
            Some(account) => Self::recover_named_k1(
                account,
                tx.payer_signature_hash(sender),
                signed.payer_auth(),
            )?,
        };

        let delegation = Self::delegation(signed, sender)?;
        Ok(AuthorizedTransaction { sender, payer, delegation })
    }

    /// Checks the authentication shape of `signed` without verifying any
    /// signature: a self-paid transaction carries no `payer_auth`, and a named
    /// sender or named payer selects [`K1_AUTHENTICATOR`].
    ///
    /// Gas estimation runs this in place of [`Self::authorize`], so a
    /// transaction it would reject is not priced.
    ///
    /// [`K1_AUTHENTICATOR`]: Eip8130Constants::K1_AUTHENTICATOR
    ///
    /// # Errors
    /// Returns [`TxAuthError::UnexpectedPayerAuth`] or
    /// [`TxAuthError::UnsupportedAuthenticator`].
    pub fn check(signed: &Eip8130Signed) -> Result<(), TxAuthError> {
        let tx = signed.tx();
        if tx.payer.is_none() && !signed.payer_auth().is_empty() {
            return Err(TxAuthError::UnexpectedPayerAuth);
        }
        let named_k1 =
            |auth: &[u8]| auth.starts_with(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        if (tx.sender.is_some() && !named_k1(signed.sender_auth()))
            || (tx.payer.is_some() && !tx.is_open_payer() && !named_k1(signed.payer_auth()))
        {
            return Err(TxAuthError::UnsupportedAuthenticator);
        }
        Ok(())
    }

    /// The delegation `sender`'s delegation entry sets or clears, if `signed`
    /// carries one. Not yet installed.
    ///
    /// # Errors
    /// Returns [`TxAuthError::MultipleDelegations`] when `signed` carries more
    /// than one delegation entry.
    pub fn delegation(
        signed: &Eip8130Signed,
        sender: Address,
    ) -> Result<Option<DelegationEffect>, TxAuthError> {
        let mut delegation = None;
        for AccountChange::Delegation(Delegation { target }) in &signed.tx().account_changes {
            if delegation.is_some() {
                return Err(TxAuthError::MultipleDelegations);
            }
            delegation = Some(DelegationEffect::new(sender, *target));
        }
        Ok(delegation)
    }

    /// Recovers `auth` (`K1_AUTHENTICATOR || r || s || v`) over `hash` and
    /// requires it to be `account`.
    fn recover_named_k1(account: Address, hash: B256, auth: &[u8]) -> Result<Address, TxAuthError> {
        let Some(signature) = auth.strip_prefix(Eip8130Constants::K1_AUTHENTICATOR.as_slice())
        else {
            return Err(TxAuthError::UnsupportedAuthenticator);
        };
        let recovered = Eip8130Signed::recover_raw_k1(hash, signature)
            .map_err(|_| TxAuthError::SignatureRecovery)?;
        if recovered != account {
            return Err(TxAuthError::SignerMismatch);
        }
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address};
    use base_common_consensus::TxEip8130;
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;
    use crate::test_utils::{K1, addr, auth_blob, key, sig, tx_with};

    /// Resolves the sender of `signed` and authorizes it.
    fn authorize(signed: &Eip8130Signed) -> Result<AuthorizedTransaction, TxAuthError> {
        let sender = ResolvedSender::resolve(signed)?;
        TransactionAuthorizer::authorize(signed, sender)
    }

    /// Signs `tx` as an EOA (bare `sender_auth`, no payer).
    fn eoa_signed(tx: TxEip8130, sender: &K256SigningKey) -> Eip8130Signed {
        let hash = tx.sender_signature_hash();
        Eip8130Signed::new(tx, Bytes::from(sig(sender, hash)), Bytes::new())
    }

    /// Signs a named-sender transaction with native-k1 auth blobs.
    fn named_signed(
        tx: TxEip8130,
        sender: &K256SigningKey,
        payer: Option<&K256SigningKey>,
    ) -> Eip8130Signed {
        let sender_hash = tx.sender_signature_hash();
        let payer_auth = payer.map_or_else(Bytes::new, |payer| {
            let sender_account = tx.sender.expect("named sender");
            auth_blob(K1, &sig(payer, tx.payer_signature_hash(sender_account)))
        });
        Eip8130Signed::new(tx, auth_blob(K1, &sig(sender, sender_hash)), payer_auth)
    }

    /// Delegation and native k1 keys (named, bare, or an open payer's raw
    /// signature) authorize; every other authenticator is unsupported.
    #[test]
    fn authorizes_delegation_and_k1_keys() {
        let sender_key = key(0x61);
        let payer_key = key(0x62);
        let sender = addr(&sender_key);
        let payer = addr(&payer_key);
        let p256 = address!("0x0000000000000000000000000000000000000100");
        let delegation = AccountChange::Delegation(Delegation { target: payer });

        let eoa = eoa_signed(tx_with(None, None, vec![delegation]), &sender_key);
        let authorized = authorize(&eoa).unwrap();
        assert_eq!(authorized.sender, sender);
        assert_eq!(authorized.payer, sender);
        assert_eq!(authorized.delegation, Some(DelegationEffect::new(sender, payer)));

        let named = named_signed(
            tx_with(Some(sender), Some(payer), Vec::new()),
            &sender_key,
            Some(&payer_key),
        );
        let authorized = authorize(&named).unwrap();
        assert_eq!(authorized.sender, sender);
        assert_eq!(authorized.payer, payer);
        assert_eq!(authorized.delegation, None);

        let open_tx = tx_with(None, Some(Eip8130Constants::OPEN_PAYER), Vec::new());
        let payer_auth = sig(&payer_key, open_tx.payer_signature_hash(sender));
        let sender_auth = sig(&sender_key, open_tx.sender_signature_hash());
        let open = Eip8130Signed::new(open_tx, Bytes::from(sender_auth), Bytes::from(payer_auth));
        assert_eq!(authorize(&open).unwrap().payer, payer);

        let p256_sender = Eip8130Signed::new(
            tx_with(Some(sender), None, Vec::new()),
            auth_blob(p256, &[0; 64]),
            Bytes::new(),
        );
        assert_eq!(
            TransactionAuthorizer::check(&p256_sender),
            Err(TxAuthError::UnsupportedAuthenticator)
        );
        assert_eq!(authorize(&p256_sender), Err(TxAuthError::UnsupportedAuthenticator));

        let p256_payer = Eip8130Signed::new(
            tx_with(None, Some(payer), Vec::new()),
            Bytes::from(sig(&sender_key, B256::ZERO)),
            auth_blob(p256, &[0; 64]),
        );
        assert_eq!(
            TransactionAuthorizer::check(&p256_payer),
            Err(TxAuthError::UnsupportedAuthenticator)
        );
    }

    /// A named sender or payer must sign with its own key.
    #[test]
    fn named_signer_must_recover_to_the_named_account() {
        let sender_key = key(0x64);
        let other_key = key(0x65);
        let sender = addr(&sender_key);

        let wrong_sender = named_signed(tx_with(Some(sender), None, Vec::new()), &other_key, None);
        assert_eq!(authorize(&wrong_sender), Err(TxAuthError::SignerMismatch));

        let payer = addr(&key(0x66));
        let wrong_payer = named_signed(
            tx_with(Some(sender), Some(payer), Vec::new()),
            &sender_key,
            Some(&other_key),
        );
        assert_eq!(authorize(&wrong_payer), Err(TxAuthError::SignerMismatch));

        let truncated = Eip8130Signed::new(
            tx_with(Some(sender), None, Vec::new()),
            auth_blob(K1, &[0x1b; 64]),
            Bytes::new(),
        );
        assert_eq!(authorize(&truncated), Err(TxAuthError::SignatureRecovery));
    }

    #[test]
    fn multiple_delegations_are_rejected() {
        let delegation = AccountChange::Delegation(Delegation { target: Address::ZERO });
        let signed =
            eoa_signed(tx_with(None, None, vec![delegation.clone(), delegation]), &key(0x67));
        assert_eq!(authorize(&signed), Err(TxAuthError::MultipleDelegations));
    }

    /// A self-paid transaction carrying `payer_auth` is rejected.
    #[test]
    fn self_pay_with_payer_auth_is_rejected() {
        let sender_key = key(0x63);
        let tx = tx_with(None, None, Vec::new());
        let sender_auth = sig(&sender_key, tx.sender_signature_hash());
        let padded =
            Eip8130Signed::new(tx, Bytes::from(sender_auth), Bytes::from_static(&[0xab; 32]));

        assert_eq!(TransactionAuthorizer::check(&padded), Err(TxAuthError::UnexpectedPayerAuth));
        assert_eq!(authorize(&padded), Err(TxAuthError::UnexpectedPayerAuth));
    }

    #[test]
    fn sender_resolved_from_another_transaction_is_rejected() {
        let first = eoa_signed(TxEip8130::default(), &key(0x11));
        let second = eoa_signed(TxEip8130::default(), &key(0x22));
        let sender = ResolvedSender::resolve(&first).unwrap();

        assert_eq!(
            TransactionAuthorizer::authorize(&second, sender),
            Err(TxAuthError::ResolvedSenderMismatch)
        );
    }
}
