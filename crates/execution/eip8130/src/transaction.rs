//! The EIP-8130 transaction-authorization orchestrator: the final sender/payer
//! signatures plus the transaction's ordered account-configuration changes.

use base_common_consensus::{AccountChange, Eip8130Signed};

use crate::{
    AccountConfigurationStorage, ActorTxVerifier, AuthorizeError, ConfigChangeAuthorizer,
    ResolvedActor, TxActors, TxAuthError,
};

/// The authorized result of an EIP-8130 transaction: its resolved actors and the
/// authorizing actor of each account-configuration change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthorizedTransaction {
    /// The transaction's sender and (optional) payer, each scope-gated.
    pub actors: TxActors,
    /// The authorizing actor resolved for each [`ConfigChange`] entry, in
    /// transaction order. Empty when the transaction carries no config changes.
    ///
    /// [`ConfigChange`]: base_common_consensus::ConfigChange
    pub config_changes: Vec<ResolvedActor>,
}

/// Authorizes a signed EIP-8130 transaction against an
/// [`AccountConfigurationStorage`] view, composing the sender/payer signature
/// checks with the ordered authorization of its account-configuration changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransactionAuthorizer;

impl TransactionAuthorizer {
    /// Authorizes `signed` against `storage` for `local_chain_id` at time `now`
    /// (block timestamp at inclusion, wall-clock in the pool, used for actor
    /// expiry and the account lock).
    ///
    /// Resolves and scope-gates the sender (and payer), then walks
    /// `account_changes` in order, authorizing each [`ConfigChange`] with its own
    /// `SignedActorChanges` signature. Same-channel entries advance their channel
    /// sequence by one each within the transaction, so entries are validated
    /// against the running per-channel sequence rather than re-reading state.
    ///
    /// Returns the [`AuthorizedTransaction`], or the first [`TxAuthError`]
    /// encountered (sender, then payer, then config changes in order). Reads
    /// state but performs no mutations.
    ///
    /// [`ConfigChange`]: base_common_consensus::ConfigChange
    pub fn authorize(
        signed: &Eip8130Signed,
        storage: &AccountConfigurationStorage<'_>,
        local_chain_id: u64,
        now: u64,
    ) -> Result<AuthorizedTransaction, TxAuthError> {
        // 1. The final transaction signatures over the whole body. This also
        //    resolves the sender account (recovering it for the EOA path), which
        //    the config changes apply against.
        let actors = ActorTxVerifier::verify(signed, storage, now)?;
        let account = actors.sender.account;

        // 2. Read each config-change channel's base sequence once, then validate
        //    entries against `base + applied`, where `applied` counts the
        //    same-channel entries already authorized in this transaction (each
        //    applied entry advances its channel by one). The running value is
        //    used rather than re-reading the (still-unwritten) state. This is an
        //    admission-time ordering check only: authorization writes nothing,
        //    and the execution/apply layer re-validates each entry against the
        //    committed sequence before advancing it.
        let (multichain_base, local_base) =
            storage.get_change_sequences(account).map_err(AuthorizeError::Storage)?;
        let mut multichain_applied = 0u64;
        let mut local_applied = 0u64;

        let mut config_changes = Vec::new();
        for change in &signed.tx().account_changes {
            // Create and Delegation entries carry no independent authorization:
            // the sender signature commits to the whole body, and a Create is
            // additionally bound by its deterministic deploy address (checked at
            // execution). Only ConfigChange carries its own actor signature.
            let AccountChange::ConfigChange(cc) = change else {
                continue;
            };

            // Select the sequence channel by chain binding, mirroring
            // `authorize_at_sequence`: `chain_id == 0` is the portable multichain
            // channel, the local chain is the chain-local channel, and any other
            // (foreign) `chain_id` is rejected here. Validating the binding before
            // bucketing keeps each running counter bound to exactly one channel —
            // a foreign `chain_id` can never share (and silently advance) the
            // local counter, even if the transaction type is later extended to
            // carry config changes for several distinct non-zero chains.
            let (base, applied) = match cc.chain_id {
                0 => (multichain_base, &mut multichain_applied),
                id if id == local_chain_id => (local_base, &mut local_applied),
                got => return Err(TxAuthError::ConfigChainId { expected: local_chain_id, got }),
            };
            // `checked_add` rejects the (unreachable) case where the channel
            // would advance past `u64::MAX` rather than wrapping or saturating
            // into a state that re-accepts a duplicate sequence.
            let expected = base.checked_add(*applied).ok_or(TxAuthError::ConfigSequenceOverflow)?;

            let resolved = ConfigChangeAuthorizer::authorize_at_sequence(
                storage,
                account,
                local_chain_id,
                cc,
                expected,
                now,
            )?;
            config_changes.push(resolved);
            *applied += 1;
        }

        Ok(AuthorizedTransaction { actors, config_changes })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
    use base_common_consensus::{
        ActorChange, ActorChangeType, ConfigChange, CreateEntry, Delegation, Eip8130Constants,
        TxEip8130,
    };
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;
    use crate::{ConfigChangeAuthorizer, Operation};

    const NOW: u64 = 1_000;
    const LOCAL: u64 = 8453;
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;

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

    /// Canonical Solidity packing of `AccountState` (`multichain`, `local`
    /// sequences and `unlocks_at`).
    fn pack_state(multichain: u64, local: u64, unlocks_at: u64) -> U256 {
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&multichain.to_be_bytes());
        b[16..24].copy_from_slice(&local.to_be_bytes());
        b[11..16].copy_from_slice(&unlocks_at.to_be_bytes()[3..]);
        U256::from_be_bytes(b)
    }

    fn revoke(actor_byte: u8) -> ActorChange {
        ActorChange {
            change_type: ActorChangeType::Revoke,
            actor_id: B256::repeat_byte(actor_byte),
            data: Bytes::new(),
        }
    }

    /// A [`ConfigChange`] whose `auth` is a fresh signature over its own digest.
    fn signed_change(
        account: Address,
        authenticator: Address,
        signer: &K256SigningKey,
        chain_id: u64,
        sequence: u64,
        actor_changes: Vec<ActorChange>,
    ) -> ConfigChange {
        let mut change = ConfigChange { chain_id, sequence, actor_changes, auth: Bytes::new() };
        let digest = ConfigChangeAuthorizer::signed_actor_changes_digest(account, &change);
        change.auth = auth_blob(authenticator, &sig(signer, digest));
        change
    }

    fn tx_with(
        sender: Option<Address>,
        payer: Option<Address>,
        account_changes: Vec<AccountChange>,
    ) -> TxEip8130 {
        TxEip8130 {
            chain_id: LOCAL,
            sender,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes,
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer,
        }
    }

    /// Signs `tx` as an EOA (bare `sender_auth`, no payer) and authorizes it.
    fn eoa_signed(tx: TxEip8130, sender: &K256SigningKey) -> Eip8130Signed {
        let hash = tx.sender_signature_hash();
        Eip8130Signed::new(tx, Bytes::from(sig(sender, hash)), Bytes::new())
    }

    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    #[test]
    fn authorizes_sequential_same_channel_config_changes() {
        let k = key(0x11);
        let account = addr(&k);
        // Two multichain entries: the second is checked against `base + 1`.
        let cc0 = signed_change(account, K1, &k, 0, 0, vec![revoke(0xa0)]);
        let cc1 = signed_change(account, K1, &k, 0, 1, vec![revoke(0xa1)]);
        let signed = eoa_signed(
            tx_with(
                None,
                None,
                vec![AccountChange::ConfigChange(cc0), AccountChange::ConfigChange(cc1)],
            ),
            &k,
        );
        with_storage(|acc| {
            let out = TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW).unwrap();
            assert_eq!(out.actors.sender.account, account);
            assert!(out.actors.payer.is_none());
            assert_eq!(out.config_changes.len(), 2);
            assert!(out.config_changes.iter().all(ResolvedActor::is_unrestricted));
        });
    }

    #[test]
    fn stale_second_same_channel_entry_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        // Both entries claim sequence 0; the second must fail once the channel
        // has advanced to 1.
        let cc0 = signed_change(account, K1, &k, 0, 0, vec![revoke(0xa0)]);
        let stale = signed_change(account, K1, &k, 0, 0, vec![revoke(0xa1)]);
        let signed = eoa_signed(
            tx_with(
                None,
                None,
                vec![AccountChange::ConfigChange(cc0), AccountChange::ConfigChange(stale)],
            ),
            &k,
        );
        with_storage(|acc| {
            assert_eq!(
                TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW),
                Err(TxAuthError::ConfigSequence { expected: 1, got: 0 }),
            );
        });
    }

    #[test]
    fn multichain_and_local_channels_advance_independently() {
        let k = key(0x11);
        let account = addr(&k);
        // multichain#0, local#0, multichain#1 — each channel counted separately.
        let m0 = signed_change(account, K1, &k, 0, 0, vec![revoke(0xa0)]);
        let l0 = signed_change(account, K1, &k, LOCAL, 0, vec![revoke(0xb0)]);
        let m1 = signed_change(account, K1, &k, 0, 1, vec![revoke(0xa1)]);
        let signed = eoa_signed(
            tx_with(
                None,
                None,
                vec![
                    AccountChange::ConfigChange(m0),
                    AccountChange::ConfigChange(l0),
                    AccountChange::ConfigChange(m1),
                ],
            ),
            &k,
        );
        with_storage(|acc| {
            let out = TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW).unwrap();
            assert_eq!(out.config_changes.len(), 3);
        });
    }

    #[test]
    fn foreign_chain_config_change_is_rejected_without_advancing_a_channel() {
        let k = key(0x11);
        let account = addr(&k);
        // A valid multichain entry followed by one bound to a foreign chain
        // (neither 0 nor LOCAL). The orchestrator rejects the foreign entry at
        // channel selection — it must not be bucketed into the local channel —
        // surfacing `ConfigChainId` rather than silently advancing a counter.
        const FOREIGN: u64 = LOCAL + 1;
        let m0 = signed_change(account, K1, &k, 0, 0, vec![revoke(0xa0)]);
        let foreign = signed_change(account, K1, &k, FOREIGN, 0, vec![revoke(0xb0)]);
        let signed = eoa_signed(
            tx_with(
                None,
                None,
                vec![AccountChange::ConfigChange(m0), AccountChange::ConfigChange(foreign)],
            ),
            &k,
        );
        with_storage(|acc| {
            assert_eq!(
                TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW),
                Err(TxAuthError::ConfigChainId { expected: LOCAL, got: FOREIGN }),
            );
        });
    }

    #[test]
    fn channel_sequence_overflow_is_rejected_not_wrapped() {
        let k = key(0x11);
        let account = addr(&k);
        // The first entry sits at the channel's max sequence; authorizing a
        // second same-channel entry would advance past u64::MAX, which must be
        // rejected rather than wrapping back to a duplicate-accepting state.
        let at_max = signed_change(account, K1, &k, 0, u64::MAX, vec![revoke(0xa0)]);
        let next = signed_change(account, K1, &k, 0, u64::MAX, vec![revoke(0xa1)]);
        let signed = eoa_signed(
            tx_with(
                None,
                None,
                vec![AccountChange::ConfigChange(at_max), AccountChange::ConfigChange(next)],
            ),
            &k,
        );
        with_storage(|acc| {
            acc.account_state.at_mut(&account).write(pack_state(u64::MAX, 0, 0)).unwrap();
            assert_eq!(
                TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW),
                Err(TxAuthError::ConfigSequenceOverflow),
            );
        });
    }

    #[test]
    fn create_and_delegation_entries_do_not_consume_a_sequence() {
        let k = key(0x11);
        let account = addr(&k);
        // A Create and a Delegation bracket a single multichain config change at
        // sequence 0; the non-config entries are authorized by the sender
        // signature alone and must not advance (or consume) the channel.
        let create = AccountChange::Create(CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: Vec::new(),
        });
        let delegation = AccountChange::Delegation(Delegation { target: account });
        let cc = signed_change(account, K1, &k, 0, 0, vec![revoke(0xa0)]);
        let signed = eoa_signed(
            tx_with(None, None, vec![create, AccountChange::ConfigChange(cc), delegation]),
            &k,
        );
        with_storage(|acc| {
            let out = TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW).unwrap();
            assert_eq!(out.config_changes.len(), 1);
        });
    }

    #[test]
    fn sender_failure_short_circuits_config_changes() {
        let sk = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let sid = actor_id(addr(&sk));
        // A config change that would itself authorize, but the sender gate fails
        // first, so the config stage is never reached.
        let cc = signed_change(account, K1, &sk, 0, 0, vec![revoke(0xa0)]);
        let tx = tx_with(Some(account), None, vec![AccountChange::ConfigChange(cc)]);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&sk, hash)), Bytes::new());
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&sid)
                .at_mut(&account)
                .write(pack(K1, Eip8130Constants::SCOPE_PAYER, 0, 0))
                .unwrap();
            assert_eq!(
                TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW),
                Err(TxAuthError::Scope {
                    operation: Operation::Sender,
                    scope: Eip8130Constants::SCOPE_PAYER,
                }),
            );
        });
    }

    #[test]
    fn composes_configured_sender_payer_and_config_change() {
        let sk = key(0x22);
        let sender_account = address!("0x00000000000000000000000000000000000000aa");
        let sid = actor_id(addr(&sk));
        let pk = key(0x33);
        let payer_account = address!("0x00000000000000000000000000000000000000cc");
        let pid = actor_id(addr(&pk));
        let ck = key(0x44);
        let cid = actor_id(addr(&ck));

        let cc = signed_change(sender_account, K1, &ck, 0, 0, vec![revoke(0xa0)]);
        let tx = tx_with(
            Some(sender_account),
            Some(payer_account),
            vec![AccountChange::ConfigChange(cc)],
        );
        let sender_hash = tx.sender_signature_hash();
        let payer_hash = tx.payer_signature_hash(sender_account);
        let signed = Eip8130Signed::new(
            tx,
            auth_blob(K1, &sig(&sk, sender_hash)),
            auth_blob(K1, &sig(&pk, payer_hash)),
        );
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&sid)
                .at_mut(&sender_account)
                .write(pack(K1, Eip8130Constants::SCOPE_SENDER, 0, 0))
                .unwrap();
            acc.actor_config
                .at_mut(&pid)
                .at_mut(&payer_account)
                .write(pack(K1, Eip8130Constants::SCOPE_PAYER, 0, 0))
                .unwrap();
            acc.actor_config
                .at_mut(&cid)
                .at_mut(&sender_account)
                .write(pack(K1, Eip8130Constants::SCOPE_CONFIG, 0, 0))
                .unwrap();
            let out = TransactionAuthorizer::authorize(&signed, acc, LOCAL, NOW).unwrap();
            assert_eq!(out.actors.sender.account, sender_account);
            assert_eq!(out.actors.payer.expect("payer present").account, payer_account);
            assert_eq!(out.config_changes.len(), 1);
            assert_eq!(out.config_changes[0].scope, Eip8130Constants::SCOPE_CONFIG);
        });
    }
}
