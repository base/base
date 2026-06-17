//! Transaction-level actor authorization: resolve and scope-gate the sender and
//! payer actors of an [`Eip8130Signed`].

use alloy_primitives::Address;
use base_common_consensus::Eip8130Signed;

use crate::{AccountConfigurationStorage, ActorAuthorizer, Operation, ResolvedActor, TxAuthError};

/// A resolved transaction actor together with the account it was authorized
/// against (the sender or payer account, not the actor id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthorizedActor {
    /// The account the actor was authorized against.
    pub account: Address,
    /// The resolved actor and its authorization surface.
    pub resolved: ResolvedActor,
}

/// The authorized actors of an EIP-8130 transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxActors {
    /// The transaction sender, scope-gated for [`Operation::Sender`].
    pub sender: AuthorizedActor,
    /// The gas payer, scope-gated for [`Operation::Payer`], or `None` when the
    /// sender pays (`tx.payer == None`).
    pub payer: Option<AuthorizedActor>,
}

/// Authorizes the sender and payer actors of an [`Eip8130Signed`] against the
/// [`AccountConfigurationStorage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActorTxVerifier;

impl ActorTxVerifier {
    /// Resolves and scope-gates the transaction's sender and (optional) payer.
    ///
    /// `now` is the timestamp used for actor expiry (block timestamp at
    /// inclusion, wall-clock in the pool). Returns the authorized [`TxActors`],
    /// or the first [`TxAuthError`] encountered (sender checked before payer).
    pub fn verify(
        signed: &Eip8130Signed,
        storage: &AccountConfigurationStorage<'_>,
        now: u64,
    ) -> Result<TxActors, TxAuthError> {
        let tx = signed.tx();
        let sender = Self::verify_sender(signed, storage, now)?;

        let payer = match tx.payer {
            None => None,
            Some(account) => {
                // The payer digest binds to the resolved sender account.
                let hash = tx.payer_signature_hash(sender.account);
                let resolved = Self::authorize_scoped(
                    storage,
                    account,
                    hash,
                    signed.payer_auth(),
                    Operation::Payer,
                    now,
                )?;
                Some(AuthorizedActor { account, resolved })
            }
        };

        Ok(TxActors { sender, payer })
    }

    /// Resolves and scope-gates the sender, handling both the configured-account
    /// path (`tx.sender == Some`) and the EOA path (`tx.sender == None`).
    fn verify_sender(
        signed: &Eip8130Signed,
        storage: &AccountConfigurationStorage<'_>,
        now: u64,
    ) -> Result<AuthorizedActor, TxAuthError> {
        let hash = signed.tx().sender_signature_hash();

        if let Some(account) = signed.explicit_sender() {
            // Configured account: `sender_auth` is already `authenticator(20) || data`.
            let resolved = Self::authorize_scoped(
                storage,
                account,
                hash,
                signed.sender_auth(),
                Operation::Sender,
                now,
            )?;
            return Ok(AuthorizedActor { account, resolved });
        }

        // EOA path: recover the sender with the checked (EIP-2) recovery, then
        // authorize the implicit-EOA owner. `sender_auth` is a bare 65-byte
        // signature, so synthesize the `address(0)` authenticator prefix that the
        // unified authorize step expects.
        let account = signed
            .recover_eoa_sender()
            .map_err(|_| TxAuthError::SenderRecovery)?
            .ok_or(TxAuthError::SenderRecovery)?;

        let mut auth = Vec::with_capacity(Address::len_bytes() + signed.sender_auth().len());
        auth.extend_from_slice(Address::ZERO.as_slice());
        auth.extend_from_slice(signed.sender_auth());

        let resolved =
            Self::authorize_scoped(storage, account, hash, &auth, Operation::Sender, now)?;
        Ok(AuthorizedActor { account, resolved })
    }

    /// Authorizes `auth` against `account`/`hash`, then enforces that the
    /// resolved actor's scope grants `operation`.
    fn authorize_scoped(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        hash: alloy_primitives::B256,
        auth: &[u8],
        operation: Operation,
        now: u64,
    ) -> Result<ResolvedActor, TxAuthError> {
        let resolved = ActorAuthorizer::authenticate_actor(storage, account, hash, auth, now)?;
        if !operation.is_granted(&resolved) {
            return Err(TxAuthError::Scope { operation, scope: resolved.scope });
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, U256, address, keccak256};
    use base_common_consensus::{Eip8130Constants, TxEip8130};
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;
    use crate::AuthorizeError;

    const NOW: u64 = 1_000;
    const ECRECOVER: Address = Eip8130Constants::ECRECOVER_AUTHENTICATOR;

    fn key(byte: u8) -> K256SigningKey {
        K256SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    fn addr(key: &K256SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    fn actor_id(account: Address) -> B256 {
        AccountConfigurationStorage::self_actor_id(account)
    }

    /// 65-byte `r || s || v` signature over `hash`, `v` in `{27, 28}`, low-s.
    fn sig(key: &K256SigningKey, hash: B256) -> Vec<u8> {
        let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = vec![0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    /// `authenticator(20) || data`.
    fn auth_blob(authenticator: Address, data: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(20 + data.len());
        out.extend_from_slice(authenticator.as_slice());
        out.extend_from_slice(data);
        Bytes::from(out)
    }

    /// Canonical Solidity packing of `ActorConfig`.
    fn pack(authenticator: Address, scope: u8, expiry: u64, policy_type: u8) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(scope) << 160)
            | (U256::from(expiry) << 168)
            | (U256::from(policy_type) << 216)
    }

    fn base_tx(sender: Option<Address>, payer: Option<Address>) -> TxEip8130 {
        TxEip8130 {
            chain_id: 8453,
            sender,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer,
        }
    }

    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    #[test]
    fn eoa_sender_authorizes_as_unrestricted_owner() {
        let k = key(0x11);
        let account = addr(&k);
        let tx = base_tx(None, None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, Bytes::from(sig(&k, hash)), Bytes::new());
        with_storage(|acc| {
            let actors = ActorTxVerifier::verify(&signed, acc, NOW).unwrap();
            assert_eq!(actors.sender.account, account);
            assert!(actors.sender.resolved.is_unrestricted());
            assert!(actors.payer.is_none());
        });
    }

    #[test]
    fn configured_sender_resolves_with_scope() {
        let k = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let id = actor_id(addr(&k));
        let tx = base_tx(Some(account), None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(ECRECOVER, &sig(&k, hash)), Bytes::new());
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_SENDER, 0, 0))
                .unwrap();
            let actors = ActorTxVerifier::verify(&signed, acc, NOW).unwrap();
            assert_eq!(actors.sender.account, account);
            assert_eq!(actors.sender.resolved.scope, Eip8130Constants::SCOPE_SENDER);
            assert!(actors.payer.is_none());
        });
    }

    #[test]
    fn sender_without_sender_scope_is_rejected() {
        let k = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let id = actor_id(addr(&k));
        let tx = base_tx(Some(account), None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(ECRECOVER, &sig(&k, hash)), Bytes::new());
        with_storage(|acc| {
            // Bound, non-zero scope that lacks SCOPE_SENDER.
            acc.actor_config
                .at_mut(&id)
                .at_mut(&account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_PAYER, 0, 0))
                .unwrap();
            assert_eq!(
                ActorTxVerifier::verify(&signed, acc, NOW),
                Err(TxAuthError::Scope {
                    operation: Operation::Sender,
                    scope: Eip8130Constants::SCOPE_PAYER,
                }),
            );
        });
    }

    #[test]
    fn sponsored_payer_resolves_against_payer_hash() {
        let sk = key(0x22);
        let sender_account = address!("0x00000000000000000000000000000000000000aa");
        let sid = actor_id(addr(&sk));
        let pk = key(0x33);
        let payer_account = address!("0x00000000000000000000000000000000000000cc");
        let pid = actor_id(addr(&pk));

        let tx = base_tx(Some(sender_account), Some(payer_account));
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            auth_blob(ECRECOVER, &sig(&sk, sender_hash)),
            auth_blob(ECRECOVER, &sig(&pk, payer_hash)),
        );
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&sid)
                .at_mut(&sender_account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_SENDER, 0, 0))
                .unwrap();
            acc.actor_config
                .at_mut(&pid)
                .at_mut(&payer_account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_PAYER, 0, 0))
                .unwrap();
            let actors = ActorTxVerifier::verify(&signed, acc, NOW).unwrap();
            let payer = actors.payer.expect("payer present");
            assert_eq!(payer.account, payer_account);
            assert_eq!(payer.resolved.scope, Eip8130Constants::SCOPE_PAYER);
        });
    }

    #[test]
    fn eoa_sender_with_sponsored_payer_binds_recovered_address() {
        // The sender is wire-invisible (tx.sender == None): it must be recovered
        // before the payer digest can be computed, since `payer_signature_hash`
        // binds to the recovered sender account.
        let sk = key(0x44);
        let sender_account = addr(&sk);
        let pk = key(0x55);
        let payer_account = address!("0x00000000000000000000000000000000000000cc");
        let pid = actor_id(addr(&pk));

        let tx = base_tx(None, Some(payer_account));
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sig(&sk, sender_hash)),
            auth_blob(ECRECOVER, &sig(&pk, payer_hash)),
        );
        with_storage(|acc| {
            // Sender is an implicit-EOA owner (self-slot empty); only the payer
            // actor needs seeding.
            acc.actor_config
                .at_mut(&pid)
                .at_mut(&payer_account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_PAYER, 0, 0))
                .unwrap();
            let actors = ActorTxVerifier::verify(&signed, acc, NOW).unwrap();
            assert_eq!(actors.sender.account, sender_account);
            assert!(actors.sender.resolved.is_unrestricted());
            let payer = actors.payer.expect("payer present");
            assert_eq!(payer.account, payer_account);
            assert_eq!(payer.resolved.scope, Eip8130Constants::SCOPE_PAYER);
        });
    }

    #[test]
    fn payer_signature_bound_to_wrong_sender_is_rejected() {
        // Same as above, but the payer signs over a digest bound to a *different*
        // sender than the one recovered from the wire. The payer signature must
        // not authenticate, proving the binding is enforced.
        let sk = key(0x44);
        let pk = key(0x55);
        let payer_account = address!("0x00000000000000000000000000000000000000cc");
        let pid = actor_id(addr(&pk));
        let wrong_sender = address!("0x00000000000000000000000000000000000000ee");

        let tx = base_tx(None, Some(payer_account));
        let sender_hash = tx.sender_signature_hash();
        let wrong_payer_hash = tx.payer_signature_hash(wrong_sender);
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sig(&sk, sender_hash)),
            auth_blob(ECRECOVER, &sig(&pk, wrong_payer_hash)),
        );
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&pid)
                .at_mut(&payer_account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_PAYER, 0, 0))
                .unwrap();
            // The payer signs the wrong digest, so it recovers a different actor
            // that is not bound on the payer account.
            assert!(matches!(
                ActorTxVerifier::verify(&signed, acc, NOW),
                Err(TxAuthError::Authorize(AuthorizeError::NotBound { .. })),
            ));
        });
    }

    #[test]
    fn payer_without_payer_scope_is_rejected() {
        let sk = key(0x22);
        let sender_account = address!("0x00000000000000000000000000000000000000aa");
        let sid = actor_id(addr(&sk));
        let pk = key(0x33);
        let payer_account = address!("0x00000000000000000000000000000000000000cc");
        let pid = actor_id(addr(&pk));

        let tx = base_tx(Some(sender_account), Some(payer_account));
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            auth_blob(ECRECOVER, &sig(&sk, sender_hash)),
            auth_blob(ECRECOVER, &sig(&pk, payer_hash)),
        );
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&sid)
                .at_mut(&sender_account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_SENDER, 0, 0))
                .unwrap();
            // Payer actor bound but lacking SCOPE_PAYER.
            acc.actor_config
                .at_mut(&pid)
                .at_mut(&payer_account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_SENDER, 0, 0))
                .unwrap();
            assert_eq!(
                ActorTxVerifier::verify(&signed, acc, NOW),
                Err(TxAuthError::Scope {
                    operation: Operation::Payer,
                    scope: Eip8130Constants::SCOPE_SENDER,
                }),
            );
        });
    }

    #[test]
    fn malformed_eoa_sender_signature_is_rejected() {
        let tx = base_tx(None, None);
        // 64 bytes is one short of a valid r||s||v payload.
        let signed = Eip8130Signed::new(tx, Bytes::from(vec![0u8; 64]), Bytes::new());
        with_storage(|acc| {
            assert_eq!(
                ActorTxVerifier::verify(&signed, acc, NOW),
                Err(TxAuthError::SenderRecovery),
            );
        });
    }

    #[test]
    fn unbound_configured_sender_propagates_authorize_error() {
        let k = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let tx = base_tx(Some(account), None);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(ECRECOVER, &sig(&k, hash)), Bytes::new());
        with_storage(|acc| {
            // No actor seeded: the sender actor is not bound on the account.
            assert!(matches!(
                ActorTxVerifier::verify(&signed, acc, NOW),
                Err(TxAuthError::Authorize(AuthorizeError::NotBound { .. })),
            ));
        });
    }
}
