//! The EIP-8130 transaction-authorization orchestrator: resolves the sender and
//! payer as secp256k1 owners and records the transaction's (single, optional)
//! delegation account-code effect.

use base_common_consensus::{AccountChange, Delegation, Eip8130Signed};

use crate::{
    ActorTxVerifier, AppliedAccountChanges, ApplyError, DelegationEffect, RecoveredActorId,
    TxActors, TxAuthError,
};

/// The authorized result of an EIP-8130 transaction: its resolved actors and the
/// deferred account-*code* effect the execution layer must install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppliedTransaction {
    /// The transaction's sender and (optional) payer.
    pub actors: TxActors,
    /// The deferred account-*code* effect (a delegation indicator) the execution
    /// layer must install against the account trie.
    pub applied: AppliedAccountChanges,
}

/// Authorizes a signed EIP-8130 transaction.
///
/// With the Keystore removed there is no `AccountConfiguration` storage to read
/// or mutate: the sender and payer are recovered as full-authority secp256k1
/// owners and the only account change is an [EIP-7702]-style delegation, whose
/// deferred code effect is recorded here and installed by the execution layer.
///
/// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransactionAuthorizer;

impl TransactionAuthorizer {
    /// Authorizes `signed`, returning the resolved actors and the deferred
    /// delegation code effect (if any).
    ///
    /// A delegation is authorized by the transaction sender (the account's own
    /// key), which is exactly the sender authenticated for the transaction: the
    /// delegation targets the sender account, so authenticating the sender as a
    /// full-authority owner authorizes the delegation. Structural invariants (at
    /// most one delegation) are enforced inline. The delegation is only *recorded*
    /// here; [`DelegationEffect::install`] performs the EOA-shaped-code check and
    /// the code write in the execution layer.
    pub fn authorize_and_apply(signed: &Eip8130Signed) -> Result<AppliedTransaction, TxAuthError> {
        // Resolve the sender account up front. For the named path it is the
        // explicit wire `sender`; for the EOA path it is the recovered signer,
        // whose recovery token is threaded into the final authentication so the
        // secp256k1 ecrecover runs exactly once per transaction.
        let (sender_account, recovered_sender) = match signed.explicit_sender() {
            Some(account) => (account, None),
            None => {
                let recovered = RecoveredActorId::recover_eoa_sender(signed)
                    .map_err(|_| TxAuthError::SenderRecovery)?
                    .ok_or(TxAuthError::SenderRecovery)?;
                (recovered.address(), Some(recovered))
            }
        };

        // Record the (single, optional) delegation code effect against the sender.
        let mut applied = AppliedAccountChanges::default();
        for change in &signed.tx().account_changes {
            match change {
                AccountChange::Delegation(Delegation { target }) => {
                    if applied.delegation.is_some() {
                        return Err(ApplyError::MultipleDelegations.into());
                    }
                    applied.delegation = Some(DelegationEffect::new(sender_account, *target));
                }
            }
        }

        // Authenticate sender + payer, reusing the EOA sender token recovered
        // above (no second ecrecover).
        let actors = ActorTxVerifier::verify_with_recovered_sender(signed, recovered_sender)?;

        Ok(AppliedTransaction { actors, applied })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
    use base_common_consensus::{Delegation, Eip8130Constants, TxEip8130};
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;

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

    fn tx_with(sender: Option<Address>, account_changes: Vec<AccountChange>) -> TxEip8130 {
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
            account_changes,
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        }
    }

    fn eoa_signed(tx: TxEip8130, sender: &K256SigningKey) -> Eip8130Signed {
        let hash = tx.sender_signature_hash();
        Eip8130Signed::new(tx, Bytes::from(sig(sender, hash)), Bytes::new())
    }

    fn named_signed(tx: TxEip8130, sender: &K256SigningKey) -> Eip8130Signed {
        let hash = tx.sender_signature_hash();
        Eip8130Signed::new(tx, auth_blob(K1, &sig(sender, hash)), Bytes::new())
    }

    #[test]
    fn eoa_sender_delegation_records_effect() {
        let k = key(0x35);
        let account = addr(&k);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed =
            eoa_signed(tx_with(None, vec![AccountChange::Delegation(Delegation { target })]), &k);
        let out = TransactionAuthorizer::authorize_and_apply(&signed).unwrap();
        assert_eq!(out.actors.sender.account, account);
        assert_eq!(out.applied.delegation, Some(DelegationEffect::new(account, target)));
    }

    #[test]
    fn named_sender_delegation_records_effect() {
        let k = key(0x36);
        let account = addr(&k);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed = named_signed(
            tx_with(Some(account), vec![AccountChange::Delegation(Delegation { target })]),
            &k,
        );
        let out = TransactionAuthorizer::authorize_and_apply(&signed).unwrap();
        assert_eq!(out.actors.sender.account, account);
        assert_eq!(out.applied.delegation, Some(DelegationEffect::new(account, target)));
    }

    #[test]
    fn clear_delegation_records_zero_target_effect() {
        let k = key(0x37);
        let account = addr(&k);
        let signed = eoa_signed(
            tx_with(None, vec![AccountChange::Delegation(Delegation { target: Address::ZERO })]),
            &k,
        );
        let out = TransactionAuthorizer::authorize_and_apply(&signed).unwrap();
        assert_eq!(out.applied.delegation, Some(DelegationEffect::new(account, Address::ZERO)));
    }

    #[test]
    fn multiple_delegations_are_rejected() {
        let k = key(0x38);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed = eoa_signed(
            tx_with(
                None,
                vec![
                    AccountChange::Delegation(Delegation { target }),
                    AccountChange::Delegation(Delegation { target: Address::ZERO }),
                ],
            ),
            &k,
        );
        assert_eq!(
            TransactionAuthorizer::authorize_and_apply(&signed),
            Err(TxAuthError::Apply(ApplyError::MultipleDelegations)),
        );
    }

    #[test]
    fn named_sender_mismatch_propagates() {
        let k = key(0x39);
        let named = address!("0x00000000000000000000000000000000000000aa");
        let signed = named_signed(tx_with(Some(named), Vec::new()), &k);
        assert!(matches!(
            TransactionAuthorizer::authorize_and_apply(&signed),
            Err(TxAuthError::SenderMismatch { expected, .. }) if expected == named
        ));
    }
}
