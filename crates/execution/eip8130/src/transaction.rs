//! The EIP-8130 transaction-authorization orchestrator: interleaves the
//! application of the transaction's ordered account-configuration changes with
//! their authorization, then authenticates the final sender/payer signatures
//! against the resulting post-apply state.

use base_common_consensus::{AccountChange, Delegation, Eip8130Signed};

use crate::{
    AccountChangeApplier, AccountConfigurationStorage, ActorTxVerifier, AppliedAccountChanges,
    ApplyError, AuthorizeError, AuthorizedActor, ConfigChangeAuthorizer, DelegationEffect,
    RecoveredActorId, ResolvedActor, TxActors, TxAuthError,
};

/// The authorized-and-applied result of an EIP-8130 transaction: its resolved
/// actors, the authorizing actor of each account-configuration change, and the
/// deferred account-*code* effects the execution layer must install.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppliedTransaction {
    /// The transaction's sender and (optional) payer, each scope-gated, resolved
    /// against the state **after** every account change has been applied.
    pub actors: TxActors,
    /// The authorizing actor resolved for each [`ConfigChange`] entry, in
    /// transaction order. Empty when the transaction carries no config changes.
    ///
    /// [`ConfigChange`]: base_common_consensus::ConfigChange
    pub config_changes: Vec<ResolvedActor>,
    /// The deferred account-*code* effects (created-account bytecode, delegation
    /// indicator) the execution layer must install against the account trie. All
    /// `AccountConfiguration` *storage* transitions are already written to
    /// `storage` by the time this is returned.
    pub applied: AppliedAccountChanges,
    /// Number of empty zero-to-zero revoke slots resolved during application
    /// (see [`crate::IntrinsicGasInput::revoke_discount_slots`]). Threaded into
    /// intrinsic gas to discount the over-conservative three-reset revoke price.
    pub revoke_discount_slots: u32,
}

/// Authorizes and applies a signed EIP-8130 transaction against a mutable
/// [`AccountConfigurationStorage`] view.
///
/// The account changes are applied *interleaved* with their authorization, each
/// against the evolving working state, mirroring `AccountConfiguration`'s own
/// execution order (`createAccount` then `applySignedActorChanges`, …). The
/// sender and payer are authenticated last, against the final post-apply state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransactionAuthorizer;

impl TransactionAuthorizer {
    /// Authorizes and applies `signed` against `storage` for `local_chain_id` at
    /// time `now` (block timestamp at inclusion, wall-clock in the pool, used for
    /// actor expiry and the account lock).
    ///
    /// The flow mirrors the canonical `AccountConfiguration` contract, which
    /// applies a transaction's changes against an *evolving* state rather than a
    /// single pre-state snapshot:
    ///
    /// 1. A leading [`AccountChange::Create`] is applied first, installing the
    ///    new account's initial actors and bootstrapping its change sequence
    ///    (`local_sequence = 1`).
    /// 2. Each [`AccountChange::ConfigChange`] is authorized against the
    ///    **current** working state — so its sequence is read live (no running
    ///    counter is needed; the prior apply already advanced it) and its
    ///    authenticator binding is checked against actors installed by earlier
    ///    changes in the same transaction — then immediately applied.
    /// 3. A [`AccountChange::Delegation`] records its deferred code effect.
    /// 4. Finally the sender and payer are authenticated against the resulting
    ///    state. A transaction that revokes its own authenticating actor in an
    ///    earlier change therefore fails the final sender check, exactly as the
    ///    contract would revert. A recorded delegation then requires that final
    ///    sender to be an admin (unrestricted) actor on the unlocked account,
    ///    via any canonical authenticator.
    ///
    /// `max_code_size` is the block's active deployed-code cap (EIP-170
    /// pre-Amsterdam, EIP-7954 after), applied to a create entry's runtime so the
    /// enshrined deploy honors the same limit the EVM enforces for `CREATE`.
    ///
    /// Returns the [`AppliedTransaction`] (with every `AccountConfiguration`
    /// storage transition already written to `storage`), or the first
    /// [`TxAuthError`] encountered. On error the caller MUST discard `storage`'s
    /// pending writes (revert the journal/overlay checkpoint).
    ///
    /// [`ConfigChange`]: base_common_consensus::ConfigChange
    pub fn authorize_and_apply(
        signed: &Eip8130Signed,
        storage: &mut AccountConfigurationStorage<'_>,
        local_chain_id: u64,
        now: u64,
        max_code_size: usize,
    ) -> Result<AppliedTransaction, TxAuthError> {
        // Resolve the sender account up front (without authenticating it yet):
        // config changes mutate this account and a create must derive to it. For
        // the configured path it is the explicit wire `sender`; for the EOA path
        // it is the recovered signer. The sender's *authorization* is deferred to
        // the final post-apply check (step 4 below). The EOA recovery token is
        // kept and threaded into that final check so the secp256k1 ecrecover runs
        // exactly once per transaction rather than again inside `verify`.
        let (sender_account, recovered_sender) = match signed.explicit_sender() {
            Some(account) => (account, None),
            None => {
                let recovered = RecoveredActorId::recover_eoa_sender(signed)
                    .map_err(|_| TxAuthError::SenderRecovery)?
                    .ok_or(TxAuthError::SenderRecovery)?;
                (recovered.address(), Some(recovered))
            }
        };

        // 1–3. Walk the account changes in order, applying each against the
        //       evolving state and authorizing every config change as it is
        //       reached. Structural invariants (one create at index 0, at most
        //       one delegation) are enforced inline.
        let mut applied = AppliedAccountChanges::default();
        let mut config_changes = Vec::new();
        let mut revoke_discount_slots = 0u32;
        for (index, change) in signed.tx().account_changes.iter().enumerate() {
            match change {
                AccountChange::Create(entry) => {
                    // A create coexisting with a delegation is the same semantic
                    // violation regardless of entry order: surface it as
                    // `CreateAndDelegation` here too (the delegation arm reports
                    // it for the `Create … Delegation` order) rather than letting
                    // the position check below mask it as `InvalidCreatePosition`.
                    if applied.delegation.is_some() {
                        return Err(ApplyError::CreateAndDelegation.into());
                    }
                    if index != 0 || applied.created.is_some() {
                        return Err(ApplyError::InvalidCreatePosition.into());
                    }
                    let created =
                        AccountChangeApplier::apply_create(storage, entry, max_code_size)?;
                    if created.address != sender_account {
                        return Err(ApplyError::CreateAddressMismatch {
                            derived: created.address,
                            sender: sender_account,
                        }
                        .into());
                    }
                    applied.created = Some(created);
                }
                AccountChange::ConfigChange(cc) => {
                    // Load the evolving packed state once for this change. The
                    // same decoded value drives lock/sequence authorization,
                    // inline-self authentication and mutation, and the final
                    // sequence/self-state write.
                    let mut state = storage
                        .get_account_state(sender_account)
                        .map_err(AuthorizeError::Storage)?;
                    let resolved = ConfigChangeAuthorizer::authorize_with_account_state(
                        storage,
                        sender_account,
                        local_chain_id,
                        cc,
                        now,
                        &state,
                    )?;
                    config_changes.push(resolved);
                    // `apply_config_change_with_account_state` silently skips an
                    // already-expired grant on the replayable unsequenced (JIT)
                    // Local path (per change, so live siblings still apply);
                    // sequenced and multichain batches retain such a grant and
                    // install it inert. Nothing reverts on expiry. `now` (block
                    // seconds) drives that skip.
                    revoke_discount_slots = revoke_discount_slots.saturating_add(
                        AccountChangeApplier::apply_config_change_with_account_state(
                            storage,
                            sender_account,
                            &cc.changes,
                            cc.channel,
                            cc.sequence,
                            &mut state,
                            now,
                        )?,
                    );
                    storage
                        .set_account_state(sender_account, state)
                        .map_err(ApplyError::Storage)?;
                }
                AccountChange::Delegation(Delegation { target }) => {
                    if applied.delegation.is_some() {
                        return Err(ApplyError::MultipleDelegations.into());
                    }
                    if applied.created.is_some() {
                        return Err(ApplyError::CreateAndDelegation.into());
                    }
                    applied.delegation = Some(DelegationEffect::new(sender_account, *target));
                }
            }
        }

        // 4. Authenticate sender + payer against the final post-apply state,
        //    reusing the EOA sender token recovered above (no second ecrecover).
        let actors =
            ActorTxVerifier::verify_with_recovered_sender(signed, storage, now, recovered_sender)?;

        if applied.delegation.is_some() {
            Self::authorize_delegation(storage, now, &actors.sender)?;
        }

        Ok(AppliedTransaction { actors, config_changes, applied, revoke_discount_slots })
    }

    /// Requires a delegation's final sender to be an admin (unrestricted) actor
    /// on the unlocked account.
    ///
    /// The authorizing actor may use **any** canonical authenticator (secp256k1,
    /// P-256, `WebAuthn`, or a delegate contract): it does not need to be the
    /// account's native-k1 self actor, nor a secp256k1 actor at all. Any bound
    /// admin (`scope == 0`) actor may set the account's delegation, consistent
    /// with the admin role that already governs every other config mutation
    /// (authorize/revoke actors, including revoking the root key). The
    /// account-unlocked guard remains.
    ///
    /// This gate does **not** inspect the account's code. Delegation is
    /// independently constrained to accounts that are *currently an EOA* — no
    /// code, or an existing EIP-7702 delegation indicator — by
    /// [`DelegationEffect::can_replace_code`], enforced when
    /// [`DelegationEffect::install`] runs in both the mempool overlay and block
    /// execution (ordinary contract code is rejected with
    /// [`ApplyError::NonDelegatableCode`]). Allowing any admin actor here is
    /// therefore safe: an admin key can only (re)delegate a genuinely EOA-shaped
    /// account, never repoint a deployed smart account's code.
    ///
    /// [`DelegationEffect::can_replace_code`]: crate::DelegationEffect::can_replace_code
    /// [`DelegationEffect::install`]: crate::DelegationEffect::install
    /// [`ApplyError::NonDelegatableCode`]: crate::ApplyError::NonDelegatableCode
    fn authorize_delegation(
        storage: &AccountConfigurationStorage<'_>,
        now: u64,
        sender: &AuthorizedActor,
    ) -> Result<(), TxAuthError> {
        if storage.is_locked(sender.account, now).map_err(AuthorizeError::Storage)? {
            return Err(TxAuthError::AccountIsLocked);
        }

        if !sender.resolved.is_admin() {
            return Err(TxAuthError::DelegationUnauthorized);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
    use alloy_sol_types::{SolValue, sol};
    use base_common_consensus::{
        AccountChangeChannel, ChangeType, CreateEntry, Delegation, Eip8130Constants, InitialActor,
        SignedAccountChanges, SignedChange, TxEip8130,
    };
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey as K256SigningKey;

    sol! {
        /// ABI shape of `AuthorizeActor`'s `ActorConfig` payload field, used only
        /// to synthesize a benign change for sequence/auth tests.
        struct TestActorConfigAbi {
            address authenticator;
            uint48 expiry;
            uint16 scope;
        }
    }

    /// A minimal, always-appliable `AuthorizeActor` change (upsert of a throwaway
    /// actor). Sequence/auth tests use it so a batch is never empty — the apply
    /// path rejects an empty batch with `EmptyChangeSet`, mirroring the contract.
    fn benign_change() -> SignedChange {
        let abi = TestActorConfigAbi {
            authenticator: K1,
            expiry: alloy_primitives::aliases::U48::ZERO,
            scope: Eip8130Constants::SCOPE_OPERATOR,
        };
        let payload = (B256::repeat_byte(0xEE), abi, Bytes::new()).abi_encode_params();
        SignedChange { change_type: ChangeType::AuthorizeActor, payload: Bytes::from(payload) }
    }

    use super::*;
    use crate::{
        AccountChangeApplier, ApplyError, AuthorizeError, AuthorizedActor, ConfigChangeAuthorizer,
    };

    const NOW: u64 = 1_000;
    const LOCAL: u64 = 8453;
    /// Pre-Amsterdam (EIP-170) deployed-code cap used by the tests.
    const MAX_CODE: usize = Eip8130Constants::MAX_CODE_SIZE;
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

    /// Canonical Solidity packing of `ActorConfig` (authenticator 0..160, expiry
    /// 160..208, scope 208..224).
    fn pack(authenticator: Address, scope: u16, expiry: u64) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(expiry) << 160)
            | (U256::from(scope) << 208)
    }

    /// Canonical Solidity packing of `AccountState` (`multichain`, `local`
    /// sequences, `flags`, and the `lock_union`; `local` is the low uint32
    /// local-channel sequence, local epoch left zero).
    fn pack_state(multichain: u64, local: u64, flags: u8, lock_union: u64) -> U256 {
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&multichain.to_be_bytes());
        b[20..24].copy_from_slice(&local.to_be_bytes()[4..]); // uint32 localSequence
        b[15] = flags;
        b[9..15].copy_from_slice(&lock_union.to_be_bytes()[2..]); // uint48 lockUnion
        U256::from_be_bytes(b)
    }

    /// Packs the inline secp256k1 self actor's `default_eoa_scope` (bits
    /// 232..248).
    fn pack_default_eoa_scope(scope: u16) -> U256 {
        U256::from(scope) << 232
    }

    /// A [`SignedAccountChanges`] batch whose `signature` is a fresh signature
    /// over its own digest.
    fn signed_change(
        account: Address,
        authenticator: Address,
        signer: &K256SigningKey,
        channel: AccountChangeChannel,
        sequence: u64,
        changes: Vec<SignedChange>,
    ) -> SignedAccountChanges {
        // These tests probe sequence gating and authenticator resolution, not
        // change content, so callers pass an empty vec. The apply path now
        // rejects an empty batch (`EmptyChangeSet`), so substitute a benign,
        // always-appliable change to keep the batch non-empty.
        let changes = if changes.is_empty() { vec![benign_change()] } else { changes };
        let mut change =
            SignedAccountChanges { channel, sequence, changes, signature: Bytes::new() };
        let digest = ConfigChangeAuthorizer::changes_digest(account, LOCAL, &change);
        change.signature = auth_blob(authenticator, &sig(signer, digest));
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
            valid_after: 0,
            valid_before: 0,
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

    /// Signs an explicit-sender transaction with native-k1 auth blobs.
    fn configured_signed(
        tx: TxEip8130,
        sender: &K256SigningKey,
        payer: Option<&K256SigningKey>,
    ) -> Eip8130Signed {
        let sender_hash = tx.sender_signature_hash();
        let payer_auth = payer.map_or_else(Bytes::new, |payer| {
            let sender_account = tx.sender.expect("configured sender");
            auth_blob(K1, &sig(payer, tx.payer_signature_hash(sender_account)))
        });
        Eip8130Signed::new(tx, auth_blob(K1, &sig(sender, sender_hash)), payer_auth)
    }

    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    #[test]
    fn authorizes_sequential_same_channel_config_changes() {
        let k = key(0x11);
        let account = addr(&k);
        // Two multichain entries with empty actor changes (each still advances
        // the channel): the first applies and the second is then authorized
        // against the advanced live sequence (1).
        let cc0 = signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![]);
        let cc1 = signed_change(account, K1, &k, AccountChangeChannel::Multichain, 1, vec![]);
        let signed = eoa_signed(
            tx_with(
                None,
                None,
                vec![AccountChange::ConfigChange(cc0), AccountChange::ConfigChange(cc1)],
            ),
            &k,
        );
        with_storage(|acc| {
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();
            assert_eq!(out.actors.sender.account, account);
            assert!(out.actors.payer.is_none());
            assert_eq!(out.config_changes.len(), 2);
            assert!(out.config_changes.iter().all(ResolvedActor::is_admin));
            // Both entries applied: the multichain channel advanced by two.
            assert_eq!(acc.get_change_sequences(account).unwrap(), (2, 0));
        });
    }

    #[test]
    fn stale_second_same_channel_entry_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        // Both entries claim sequence 0; the second must fail once the first has
        // applied and advanced the channel to 1.
        let cc0 = signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![]);
        let stale = signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![]);
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
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE),
                Err(TxAuthError::BadSequence { expected: 1, got: 0 }),
            );
        });
    }

    #[test]
    fn multichain_and_local_channels_advance_independently() {
        let k = key(0x11);
        let account = addr(&k);
        // multichain#0, local#0, multichain#1 — each channel advances separately.
        let m0 = signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![]);
        let l0 = signed_change(account, K1, &k, AccountChangeChannel::Local, 0, vec![]);
        let m1 = signed_change(account, K1, &k, AccountChangeChannel::Multichain, 1, vec![]);
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
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();
            assert_eq!(out.config_changes.len(), 3);
            assert_eq!(acc.get_change_sequences(account).unwrap(), (2, 1));
        });
    }

    #[test]
    fn stale_local_epoch_config_change_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        // A Local-channel batch committing local epoch 0 against an account whose
        // local epoch has advanced to 1: rejected at the epoch gate.
        let change = signed_change(account, K1, &k, AccountChangeChannel::Local, 0, vec![]);
        let signed = eoa_signed(tx_with(None, None, vec![AccountChange::ConfigChange(change)]), &k);
        with_storage(|acc| {
            // localEpoch = 1 (uint32 at bytes 16..20 of the packed slot).
            let mut b = [0u8; 32];
            b[16..20].copy_from_slice(&1u32.to_be_bytes());
            acc.account_state.at_mut(&account).write(U256::from_be_bytes(b)).unwrap();
            assert_eq!(
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE),
                Err(TxAuthError::StaleEpoch { expected: 1, got: 0 }),
            );
        });
    }

    #[test]
    fn channel_sequence_saturation_is_rejected_not_wrapped() {
        let k = key(0x11);
        let account = addr(&k);
        // The batch sits at the multichain channel's max sequence; advancing it
        // would overflow, so the gate rejects it rather than wrapping back to a
        // duplicate-accepting state.
        let at_max =
            signed_change(account, K1, &k, AccountChangeChannel::Multichain, u64::MAX, vec![]);
        let signed = eoa_signed(tx_with(None, None, vec![AccountChange::ConfigChange(at_max)]), &k);
        with_storage(|acc| {
            acc.account_state.at_mut(&account).write(pack_state(u64::MAX, 0, 0, 0)).unwrap();
            assert_eq!(
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE),
                Err(TxAuthError::SequenceSaturated),
            );
        });
    }

    #[test]
    fn delegation_authorization_rejects_scoped_native_k1_self_actor() {
        let sender = key(0x31);
        let account = addr(&sender);
        let payer = key(0x32);
        let payer_account = addr(&payer);
        let target = address!("0x00000000000000000000000000000000000000dd");

        let ordinary_tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            valid_before: NOW + 1,
            ..tx_with(Some(account), Some(payer_account), vec![])
        };
        let ordinary = configured_signed(ordinary_tx, &sender, Some(&payer));
        let delegation_tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            valid_before: NOW + 1,
            ..tx_with(
                Some(account),
                Some(payer_account),
                vec![AccountChange::Delegation(Delegation { target })],
            )
        };
        let delegation = configured_signed(delegation_tx, &sender, Some(&payer));

        with_storage(|acc| {
            acc.account_state
                .at_mut(&account)
                .write(pack_default_eoa_scope(Eip8130Constants::SCOPE_OPERATOR))
                .unwrap();

            let ordinary =
                TransactionAuthorizer::authorize_and_apply(&ordinary, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();
            assert_eq!(ordinary.actors.sender.resolved.scope, Eip8130Constants::SCOPE_OPERATOR);
            assert_eq!(
                TransactionAuthorizer::authorize_and_apply(&delegation, acc, LOCAL, NOW, MAX_CODE),
                Err(TxAuthError::DelegationUnauthorized),
            );
        });
    }

    #[test]
    fn delegation_authorization_accepts_non_self_native_k1_admin_actor() {
        // A bound admin (unrestricted) actor that is *not* the account's native
        // self actor may now set the delegation, as long as it authenticates via
        // native secp256k1 and the account is unlocked.
        let signer = key(0x33);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let signer_id = actor_id(addr(&signer));
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed = configured_signed(
            tx_with(Some(account), None, vec![AccountChange::Delegation(Delegation { target })]),
            &signer,
            None,
        );

        with_storage(|acc| {
            acc.actors
                .at_mut(&signer_id)
                .at_mut(&account)
                .write(pack(K1, Eip8130Constants::SCOPE_UNRESTRICTED, 0))
                .unwrap();
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();
            assert_eq!(out.actors.sender.account, account);
            assert_eq!(out.actors.sender.resolved.actor_id, signer_id);
            assert_ne!(
                out.actors.sender.resolved.actor_id,
                AccountConfigurationStorage::self_actor_id(account),
                "delegating actor is intentionally not the native self actor",
            );
            assert_eq!(out.applied.delegation, Some(DelegationEffect::new(account, target)));
        });
    }

    #[test]
    fn delegation_authorization_rejects_scoped_non_self_native_k1_actor() {
        // Broadening delegation to admin actors must not leak to merely-scoped
        // (non-admin) actors. A sponsor payer covers gas and the nonce-free key
        // lets a SENDER-scoped actor pass the sender check, so authorization
        // reaches the delegation gate — which still rejects it because the
        // sender's scope is not unrestricted.
        let signer = key(0x37);
        let account = address!("0x00000000000000000000000000000000000000ab");
        let signer_id = actor_id(addr(&signer));
        let payer = key(0x38);
        let payer_account = address!("0x00000000000000000000000000000000000000ac");
        let payer_id = actor_id(addr(&payer));
        let target = address!("0x00000000000000000000000000000000000000dd");

        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            valid_before: NOW + 1,
            ..tx_with(
                Some(account),
                Some(payer_account),
                vec![AccountChange::Delegation(Delegation { target })],
            )
        };
        let signed = configured_signed(tx, &signer, Some(&payer));

        with_storage(|acc| {
            acc.actors
                .at_mut(&signer_id)
                .at_mut(&account)
                .write(pack(K1, Eip8130Constants::SCOPE_OPERATOR, 0))
                .unwrap();
            acc.actors
                .at_mut(&payer_id)
                .at_mut(&payer_account)
                .write(pack(K1, Eip8130Constants::SCOPE_SPONSOR_PAYER, 0))
                .unwrap();
            assert_eq!(
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE),
                Err(TxAuthError::DelegationUnauthorized),
            );
        });
    }

    #[test]
    fn delegation_authorization_rejects_locked_native_k1_admin_self_actor() {
        let signer = key(0x34);
        let account = addr(&signer);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed = eoa_signed(
            tx_with(None, None, vec![AccountChange::Delegation(Delegation { target })]),
            &signer,
        );

        with_storage(|acc| {
            acc.account_state
                .at_mut(&account)
                .write(pack_state(0, 0, Eip8130Constants::FLAG_LOCKED, 0))
                .unwrap();
            assert_eq!(
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE),
                Err(TxAuthError::AccountIsLocked),
            );
        });
    }

    #[test]
    fn delegation_authorization_accepts_unlocked_native_k1_admin_self_actor() {
        let signer = key(0x35);
        let account = addr(&signer);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed = eoa_signed(
            tx_with(None, None, vec![AccountChange::Delegation(Delegation { target })]),
            &signer,
        );

        with_storage(|acc| {
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();
            assert_eq!(out.applied.delegation, Some(DelegationEffect::new(account, target)));
        });
    }

    #[test]
    fn delegation_authorization_accepts_explicit_native_k1_admin_self_actor() {
        let signer = key(0x36);
        let account = addr(&signer);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed = configured_signed(
            tx_with(Some(account), None, vec![AccountChange::Delegation(Delegation { target })]),
            &signer,
            None,
        );
        assert_eq!(signed.explicit_sender(), Some(account));
        assert!(signed.sender_auth().starts_with(K1.as_slice()));

        with_storage(|acc| {
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();

            assert_eq!(out.actors.sender.account, account);
            assert_eq!(
                out.actors.sender.resolved.actor_id,
                AccountConfigurationStorage::self_actor_id(account)
            );
            assert_eq!(out.actors.sender.resolved.scope, Eip8130Constants::SCOPE_UNRESTRICTED);
            assert_eq!(out.applied.delegation, Some(DelegationEffect::new(account, target)));
        });
    }

    #[test]
    fn delegation_authorization_accepts_non_native_k1_admin_actor() {
        // Any canonical authenticator may authorize a delegation as long as the
        // resolved sender is an admin (unrestricted) actor: a non-secp256k1
        // (e.g. P-256) admin actor is now accepted.
        let account = address!("0x00000000000000000000000000000000000000aa");
        let sender =
            AuthorizedActor { account, resolved: ResolvedActor::unrestricted(actor_id(account)) };

        with_storage(|acc| {
            assert_eq!(TransactionAuthorizer::authorize_delegation(acc, NOW, &sender), Ok(()));
        });
    }

    #[test]
    fn delegation_authorization_rejects_scoped_actor() {
        // The admin gate still holds: a merely-scoped (non-admin) sender cannot
        // authorize a delegation regardless of authenticator.
        let account = address!("0x00000000000000000000000000000000000000aa");
        let sender = AuthorizedActor {
            account,
            resolved: ResolvedActor {
                actor_id: actor_id(account),
                scope: Eip8130Constants::SCOPE_OPERATOR,
                policy_target: Address::ZERO,
                expiry: 0,
            },
        };

        with_storage(|acc| {
            assert_eq!(
                TransactionAuthorizer::authorize_delegation(acc, NOW, &sender),
                Err(TxAuthError::DelegationUnauthorized),
            );
        });
    }

    #[test]
    fn create_and_delegation_in_same_tx_are_mutually_exclusive() {
        // Create and Delegation are mutually exclusive: Create establishes a
        // fresh account (code installed by the protocol from the create entry's
        // bytecode field) while Delegation modifies an *existing* account's
        // code pointer. Having both in a single transaction is undefined by the
        // spec and rejected with CreateAndDelegation.
        let k = key(0x11);
        let signer_addr = addr(&k);
        let actor_id_k = B256::from_slice(&{
            let mut id = [0u8; 32];
            id[12..].copy_from_slice(signer_addr.as_slice());
            id
        });
        let initial_actors = vec![InitialActor::owner(actor_id_k, K1)];
        let create_entry = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: initial_actors.clone(),
        };
        let derived = AccountChangeApplier::compute_address(
            create_entry.user_salt,
            create_entry.code.as_ref(),
            &initial_actors,
        )
        .expect("address derivation");

        let delegation = AccountChange::Delegation(Delegation { target: derived });
        let tx = TxEip8130 {
            chain_id: LOCAL,
            sender: Some(derived),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: vec![AccountChange::Create(create_entry), delegation],
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        };
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&k, hash)), Bytes::new());
        with_storage(|acc| {
            let err =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .expect_err("create + delegation must be rejected");
            assert!(
                matches!(err, TxAuthError::Apply(ApplyError::CreateAndDelegation)),
                "expected CreateAndDelegation, got {err:?}"
            );
        });
    }

    #[test]
    fn delegation_then_create_in_same_tx_is_rejected_as_create_and_delegation() {
        // Reverse ordering of `create_and_delegation_in_same_tx_are_mutually_exclusive`:
        // a `Delegation` preceding the `Create` is the same semantic violation and
        // must surface the same `CreateAndDelegation` error, not the position-only
        // `InvalidCreatePosition` that the create's `index != 0` check would
        // otherwise produce.
        let k = key(0x12);
        let signer_addr = addr(&k);
        let actor_id_k = B256::from_slice(&{
            let mut id = [0u8; 32];
            id[12..].copy_from_slice(signer_addr.as_slice());
            id
        });
        let initial_actors = vec![InitialActor::owner(actor_id_k, K1)];
        let create_entry = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: initial_actors.clone(),
        };
        let derived = AccountChangeApplier::compute_address(
            create_entry.user_salt,
            create_entry.code.as_ref(),
            &initial_actors,
        )
        .expect("address derivation");

        let delegation = AccountChange::Delegation(Delegation { target: derived });
        let tx = TxEip8130 {
            chain_id: LOCAL,
            sender: Some(derived),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: vec![delegation, AccountChange::Create(create_entry)],
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        };
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&k, hash)), Bytes::new());
        with_storage(|acc| {
            let err =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .expect_err("delegation + create must be rejected");
            assert!(
                matches!(err, TxAuthError::Apply(ApplyError::CreateAndDelegation)),
                "expected CreateAndDelegation, got {err:?}"
            );
        });
    }

    #[test]
    fn admin_config_actor_also_passes_final_sender_check() {
        // Account changes are authorized and applied first; the sender/payer
        // signatures are only checked against the resulting post-apply state. A
        // An admin actor authorizes config and, because unrestricted scope grants
        // sender operations, also passes the final sender check.
        let sk = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let sid = actor_id(addr(&sk));
        let cc = signed_change(account, K1, &sk, AccountChangeChannel::Multichain, 0, vec![]);
        let tx = tx_with(Some(account), None, vec![AccountChange::ConfigChange(cc)]);
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&sk, hash)), Bytes::new());
        with_storage(|acc| {
            acc.actors
                .at_mut(&sid)
                .at_mut(&account)
                .write(pack(K1, Eip8130Constants::SCOPE_UNRESTRICTED, 0))
                .unwrap();
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();
            assert!(out.config_changes[0].is_admin());
            assert_eq!(acc.get_change_sequences(account).unwrap(), (1, 0));
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

        let cc =
            signed_change(sender_account, K1, &ck, AccountChangeChannel::Multichain, 0, vec![]);
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
            acc.actors
                .at_mut(&sid)
                .at_mut(&sender_account)
                .write(pack(
                    K1,
                    Eip8130Constants::SCOPE_OPERATOR | Eip8130Constants::SCOPE_NONCE,
                    0,
                ))
                .unwrap();
            acc.actors
                .at_mut(&pid)
                .at_mut(&payer_account)
                .write(pack(K1, Eip8130Constants::SCOPE_SPONSOR_PAYER, 0))
                .unwrap();
            acc.actors
                .at_mut(&cid)
                .at_mut(&sender_account)
                .write(pack(K1, Eip8130Constants::SCOPE_UNRESTRICTED, 0))
                .unwrap();
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .unwrap();
            assert_eq!(out.actors.sender.account, sender_account);
            assert_eq!(out.actors.payer.expect("payer present").account, payer_account);
            assert_eq!(out.config_changes.len(), 1);
            assert!(out.config_changes[0].is_admin());
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Counterfactual create path
    // ─────────────────────────────────────────────────────────────────────────

    /// Builds a K1-signed `CreateEntry` whose derived address matches `signer`
    /// and a matching `TxEip8130` with `sender = derived` and the create as the
    /// first `account_changes` entry.
    fn create_tx_and_signed(
        signer: &K256SigningKey,
        extra_changes: Vec<AccountChange>,
    ) -> (Address, Eip8130Signed) {
        let signer_addr = addr(signer);
        let actor_id_val = B256::from_slice(&{
            let mut id = [0u8; 32];
            id[12..].copy_from_slice(signer_addr.as_slice());
            id
        });
        let initial_actors = vec![InitialActor::owner(actor_id_val, K1)];
        // Non-empty runtime code so the derived CREATE2 address and code hash
        // match a production-admissible transaction: the structural validator
        // rejects an empty-code create before it reaches authorize_and_apply.
        let create = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: initial_actors.clone(),
        };
        let derived = AccountChangeApplier::compute_address(
            create.user_salt,
            create.code.as_ref(),
            &initial_actors,
        )
        .expect("address derivation");

        let mut changes = vec![AccountChange::Create(create)];
        changes.extend(extra_changes);

        let tx = TxEip8130 {
            chain_id: LOCAL,
            sender: Some(derived),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: changes,
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        };
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(signer, hash)), Bytes::new());
        (derived, signed)
    }

    #[test]
    fn counterfactual_create_authorizes_on_empty_account() {
        // A counterfactual smart-account CREATE must succeed even though the
        // account does not yet exist in storage (actor_config is empty). Before
        // the fix this returned `AuthenticatorMismatch` because `resolve_bound` ran against
        // an empty account before `apply_create` could install `initial_actors`.
        let k = key(0xc1);
        let (derived, signed) = create_tx_and_signed(&k, vec![]);

        with_storage(|acc| {
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .expect("create tx on empty account must authorize");
            assert_eq!(out.actors.sender.account, derived);
            assert!(out.actors.sender.resolved.is_admin(), "create sender must be unrestricted");
            assert!(out.actors.payer.is_none());
            assert!(out.config_changes.is_empty());
        });
    }

    #[test]
    fn create_then_config_change_authorizes_against_freshly_created_account() {
        // A Create followed by a ConfigChange in the same transaction: the config
        // change must authorize against the *post-create* actor set (the initial
        // actor is installed as an unrestricted owner), proving authorize-and-apply
        // interleaves the two against an evolving state rather than reading a
        // pre-transaction snapshot where the account does not yet exist.
        let k = key(0xc5);
        let signer_addr = addr(&k);
        let actor_id_val = B256::from_slice(&{
            let mut id = [0u8; 32];
            id[12..].copy_from_slice(signer_addr.as_slice());
            id
        });
        let initial_actors = vec![InitialActor::owner(actor_id_val, K1)];
        // Non-empty code: codeless creates are rejected by the structural
        // validator before this path, and `apply_create` now enforces the same
        // `EmptyBytecode` invariant.
        let create = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: initial_actors.clone(),
        };
        let derived = AccountChangeApplier::compute_address(
            create.user_salt,
            create.code.as_ref(),
            &initial_actors,
        )
        .expect("address derivation");

        // Config change signed by the initial actor, bound to the derived account
        // at the multichain channel's first sequence.
        let cc = signed_change(derived, K1, &k, AccountChangeChannel::Multichain, 0, vec![]);
        let tx = TxEip8130 {
            chain_id: LOCAL,
            sender: Some(derived),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: vec![AccountChange::Create(create), AccountChange::ConfigChange(cc)],
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        };
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&k, hash)), Bytes::new());

        with_storage(|acc| {
            let out =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .expect("create + config change must authorize against post-create state");
            assert_eq!(out.actors.sender.account, derived);
            assert_eq!(out.config_changes.len(), 1, "config change applied after create");
            assert!(out.applied.created.is_some(), "create entry applied");
            // `apply_create` sets `local_sequence = 1` as its created/imported
            // flag; the single multichain config change then advances the
            // multichain channel to 1 — hence `(multichain, local) == (1, 1)`.
            assert_eq!(acc.get_change_sequences(derived).unwrap(), (1, 1));
        });
    }

    #[test]
    fn counterfactual_create_wrong_signer_is_rejected() {
        // A signer not in `initial_actors` must not authorize the create.
        let owner = key(0xc2);
        let attacker = key(0xc3);
        let attacker_addr = addr(&attacker);
        let actor_id_val = B256::from_slice(&{
            let mut id = [0u8; 32];
            id[12..].copy_from_slice(attacker_addr.as_slice());
            id
        });
        let initial_actors = vec![InitialActor::owner(actor_id_val, K1)];
        let create = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: initial_actors.clone(),
        };
        let derived = AccountChangeApplier::compute_address(
            create.user_salt,
            create.code.as_ref(),
            &initial_actors,
        )
        .unwrap();

        // Sign with `owner`, whose actor_id is NOT in initial_actors.
        let tx = TxEip8130 {
            chain_id: LOCAL,
            sender: Some(derived),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: vec![AccountChange::Create(create)],
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        };
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, auth_blob(K1, &sig(&owner, hash)), Bytes::new());

        with_storage(|acc| {
            assert!(
                matches!(
                    TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE),
                    Err(TxAuthError::Authorize(AuthorizeError::AuthenticatorMismatch { .. }))
                ),
                "signer not in initial_actors must be rejected"
            );
        });
    }

    #[test]
    fn counterfactual_create_without_explicit_sender_is_rejected() {
        // A create tx with `sender = None` (EOA path) must be rejected since the
        // sender address cannot be the derived CREATE2 address.
        let k = key(0xc4);
        let signer_addr = addr(&k);
        let actor_id_val = B256::from_slice(&{
            let mut id = [0u8; 32];
            id[12..].copy_from_slice(signer_addr.as_slice());
            id
        });
        let initial_actors = vec![InitialActor::owner(actor_id_val, K1)];
        let create = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors,
        };
        let tx = TxEip8130 {
            chain_id: LOCAL,
            sender: None, // missing explicit sender
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 250_000,
            account_changes: vec![AccountChange::Create(create)],
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        };
        let hash = tx.sender_signature_hash();
        let signed = Eip8130Signed::new(tx, Bytes::from(sig(&k, hash)), Bytes::new());

        with_storage(|acc| {
            assert!(
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .is_err(),
                "create without explicit sender must be rejected"
            );
        });
    }

    #[test]
    fn create_and_delegation_in_same_tx_is_rejected() {
        // A transaction must not contain both a Create entry and a Delegation
        // entry: these are mutually exclusive operations. Create establishes a
        // fresh account (code installed by the protocol); Delegation modifies
        // an existing account's code pointer. Having both is structurally invalid.
        let k = key(0xc9);
        let (_derived, mut signed) = create_tx_and_signed(&k, vec![]);
        // Append a delegation after the create.
        let delegation = AccountChange::Delegation(Delegation { target: Address::ZERO });
        let tx = signed.tx().clone();
        let mut changes = tx.account_changes.clone();
        changes.push(delegation);
        let new_tx = TxEip8130 { account_changes: changes, ..tx };
        let hash = new_tx.sender_signature_hash();
        signed = Eip8130Signed::new(new_tx, auth_blob(K1, &sig(&k, hash)), Bytes::new());

        with_storage(|acc| {
            let err =
                TransactionAuthorizer::authorize_and_apply(&signed, acc, LOCAL, NOW, MAX_CODE)
                    .expect_err("create + delegation must be rejected");
            assert!(
                matches!(err, TxAuthError::Apply(ApplyError::CreateAndDelegation)),
                "expected CreateAndDelegation, got {err:?}"
            );
        });
    }
}
