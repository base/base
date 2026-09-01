//! Account-configuration change authorization — the admin-only path of the
//! EIP-8130 validation flow.

use alloy_primitives::{Address, B256, Keccak256, b256, keccak256};
use base_common_consensus::{AccountChangeChannel, Eip8130Constants, SignedAccountChanges};

use crate::{
    AccountConfigurationStorage, AccountState, ActorAuthorizer, AuthorizeError, Operation,
    ResolvedActor, TxAuthError,
};

/// Precomputed `keccak256` typehash of the `SignedAccountChangeBatch` struct, matching
/// the one hashed by `Keystore` (the trailing `AccountChange(...)` is the
/// referenced struct's type, per the EIP-712 encoding rules):
/// `keccak256("SignedAccountChangeBatch(address account,uint256 chainId,uint64 sequence,AccountChange[] changes)AccountChange(uint8 changeType,bytes payload)")`.
/// Pinned to its preimage by `typehashes_match_their_preimages`.
const SIGNED_ACCOUNT_CHANGES_TYPEHASH: B256 =
    b256!("bee0c72c3efba751405b4c241f52736439f7e1e2a804925d36ddc9a6e1aa3614");
/// Precomputed `keccak256` typehash for the per-change `AccountChange` leaves:
/// `keccak256("AccountChange(uint8 changeType,bytes payload)")`.
/// Pinned to its preimage by `typehashes_match_their_preimages`.
const ACCOUNT_CHANGE_TYPEHASH: B256 =
    b256!("681f8ef00ffa856a78cc6a384ed51300e5805fdf59f4567ee51634dc39e0cb43");

/// Authorizes EIP-8130 signed account-change batches against an
/// [`AccountConfigurationStorage`] view.
///
/// Native mirror of `Keystore.applySignedAccountChanges`'s authorization tail:
/// it enforces the channel's epoch/sequence gate, reconstructs the
/// `SignedAccountChanges` digest, runs the batch's `signature` through the
/// stateful [`ActorAuthorizer`], and enforces the flat admin (`scope == 0`)
/// gate and the account lock. It does **not** apply the changes (decode
/// payloads, mutate `actor_config`) — that is the consuming validator's
/// responsibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfigChangeAuthorizer;

impl ConfigChangeAuthorizer {
    /// Authorize a single [`SignedAccountChanges`] batch for `account` (the
    /// resolved transaction sender, against whose config the batch applies).
    ///
    /// `now` is the timestamp used for the lock check and actor expiry (block
    /// timestamp at inclusion, wall-clock in the pool). Returns the authorizing
    /// actor's resolved surface, or the first [`TxAuthError`] encountered.
    ///
    /// Reads the account's *current* channel epoch/sequence from state; because
    /// applying a batch advances its channel, an orchestrator carrying several
    /// same-channel batches in one transaction must re-read state between
    /// batches (this authorizer always reads the live value).
    pub fn authorize(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        local_chain_id: u64,
        change: &SignedAccountChanges,
        now: u64,
    ) -> Result<ResolvedActor, TxAuthError> {
        let state = storage.get_account_state(account).map_err(AuthorizeError::Storage)?;
        Self::authorize_with_account_state(storage, account, local_chain_id, change, now, &state)
    }

    /// Authorizes a batch using an already-loaded packed account state for its
    /// lock, channel epoch/sequence gate, and inline-self authentication.
    ///
    /// Enforces (in order): the channel's epoch/sequence gate
    /// ([`AccountChangeChannel::Local`] splits `sequence` into
    /// `localEpoch(high) || localSequence(low)` with the
    /// [`Eip8130Constants::UNSEQUENCED`] JIT sentinel, while
    /// [`AccountChangeChannel::Multichain`] is a plain monotonic counter); the
    /// digest reconstruction and signature authentication; and the flat admin
    /// (`scope == 0`) gate that governs every signed account change. Per-op lock
    /// policy (`AuthorizeActor` add/re-lease above the unlock floor;
    /// `RevokeActor` rejected) is enforced in [`crate::AccountChangeApplier`].
    pub fn authorize_with_account_state(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        local_chain_id: u64,
        change: &SignedAccountChanges,
        now: u64,
        state: &AccountState,
    ) -> Result<ResolvedActor, TxAuthError> {
        Self::check_channel_sequence(change, state)?;

        // Reconstruct the digest, authenticate the signature, and require admin scope.
        let digest = Self::changes_digest(account, local_chain_id, change);
        let resolved = ActorAuthorizer::authenticate_actor_with_account_state(
            storage,
            account,
            digest,
            &change.signature,
            now,
            Some(state),
        )?;
        if !Operation::Config.is_granted(&resolved) {
            return Err(TxAuthError::Scope { operation: Operation::Config, scope: resolved.scope });
        }
        Ok(resolved)
    }

    /// Enforces the channel's epoch/sequence gate against `state`. Mirrors the
    /// epoch/sequence block at the top of `applySignedAccountChanges`.
    fn check_channel_sequence(
        change: &SignedAccountChanges,
        state: &AccountState,
    ) -> Result<(), TxAuthError> {
        match change.channel {
            AccountChangeChannel::Local => {
                let epoch = u64::from((change.sequence >> 32) as u32);
                let seq = change.sequence as u32;
                if epoch != state.local_epoch {
                    return Err(TxAuthError::StaleEpoch {
                        expected: state.local_epoch,
                        got: epoch,
                    });
                }
                // An unsequenced (JIT) batch consumes no counter; any other value
                // must match the account's current local sequence.
                if seq != Eip8130Constants::UNSEQUENCED {
                    if u64::from(seq) != state.local_sequence {
                        return Err(TxAuthError::BadSequence {
                            expected: state.local_sequence,
                            got: u64::from(seq),
                        });
                    }
                    if seq >= Eip8130Constants::UNSEQUENCED - 1 {
                        return Err(TxAuthError::SequenceSaturated);
                    }
                }
            }
            AccountChangeChannel::Multichain => {
                if change.sequence != state.multichain_sequence {
                    return Err(TxAuthError::BadSequence {
                        expected: state.multichain_sequence,
                        got: change.sequence,
                    });
                }
                if change.sequence == u64::MAX {
                    return Err(TxAuthError::SequenceSaturated);
                }
            }
        }
        Ok(())
    }

    /// Computes the EIP-8130 `SignedAccountChanges` digest for `change` against
    /// `account`, byte-identical to `Keystore._changesDigest`.
    ///
    /// The channel's replay domain sets `chainId` (`0` for
    /// [`AccountChangeChannel::Multichain`], `local_chain_id` for
    /// [`AccountChangeChannel::Local`]). Each op is hashed as
    /// `keccak256(abi.encode(ACCOUNT_CHANGE_TYPEHASH, changeType, keccak256(payload)))`,
    /// the leaf hashes are concatenated (`abi.encodePacked`) and hashed, and the
    /// result is folded into the outer struct hash.
    #[must_use]
    pub fn changes_digest(
        account: Address,
        local_chain_id: u64,
        change: &SignedAccountChanges,
    ) -> B256 {
        let chain_id = if change.channel.is_local() { local_chain_id } else { 0 };

        let mut packed = Keccak256::new();
        for op in &change.changes {
            // abi.encode(bytes32, uint8, bytes32): three right-aligned words.
            let mut leaf = [0u8; 96];
            leaf[..32].copy_from_slice(ACCOUNT_CHANGE_TYPEHASH.as_slice());
            leaf[63] = op.change_type.op_byte();
            leaf[64..96].copy_from_slice(keccak256(&op.payload).as_slice());
            packed.update(keccak256(leaf).as_slice());
        }
        let changes_hash = packed.finalize();

        // abi.encode(bytes32, address, uint256, uint64, bytes32): five words.
        let mut outer = [0u8; 160];
        outer[..32].copy_from_slice(SIGNED_ACCOUNT_CHANGES_TYPEHASH.as_slice());
        outer[44..64].copy_from_slice(account.as_slice());
        outer[88..96].copy_from_slice(&chain_id.to_be_bytes());
        outer[120..128].copy_from_slice(&change.sequence.to_be_bytes());
        outer[128..160].copy_from_slice(changes_hash.as_slice());
        keccak256(outer)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256, address};
    use base_common_consensus::{AccountChangeChannel, ChangeType, Eip8130Constants, SignedChange};
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;

    const NOW: u64 = 1_000;
    const LOCAL: u64 = 8453;
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;

    #[test]
    fn typehashes_match_their_preimages() {
        assert_eq!(
            SIGNED_ACCOUNT_CHANGES_TYPEHASH,
            keccak256(
                b"SignedAccountChangeBatch(address account,uint256 chainId,uint64 sequence,AccountChange[] changes)AccountChange(uint8 changeType,bytes payload)"
            )
        );
        assert_eq!(
            ACCOUNT_CHANGE_TYPEHASH,
            keccak256(b"AccountChange(uint8 changeType,bytes payload)")
        );
    }

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

    /// Canonical Solidity packing of `AccountState` (`local` is the low-uint32
    /// local sequence, `epoch` the adjacent uint32 local epoch).
    fn pack_state(multichain: u64, local: u64, flags: u8, lock_union: u64) -> U256 {
        pack_state_epoch(multichain, local, 0, flags, lock_union)
    }

    /// [`pack_state`] with an explicit local epoch (uint32 at bytes `16..20`).
    fn pack_state_epoch(
        multichain: u64,
        local: u64,
        epoch: u64,
        flags: u8,
        lock_union: u64,
    ) -> U256 {
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&multichain.to_be_bytes());
        b[20..24].copy_from_slice(&local.to_be_bytes()[4..]); // uint32 localSequence
        b[16..20].copy_from_slice(&epoch.to_be_bytes()[4..]); // uint32 localEpoch
        b[15] = flags;
        b[9..15].copy_from_slice(&lock_union.to_be_bytes()[2..]); // uint48 lockUnion
        U256::from_be_bytes(b)
    }

    /// A `RevokeActor` op. Payload is `abi.encode(bytes32 actorId)` (opaque to
    /// the authorizer, which only hashes it into the digest).
    fn revoke(actor_byte: u8) -> SignedChange {
        SignedChange {
            change_type: ChangeType::RevokeActor,
            payload: Bytes::copy_from_slice(B256::repeat_byte(actor_byte).as_slice()),
        }
    }

    /// An `AuthorizeActor` op carrying an opaque payload.
    fn authorize_change(actor_byte: u8, data: &[u8]) -> SignedChange {
        let mut payload = B256::repeat_byte(actor_byte).as_slice().to_vec();
        payload.extend_from_slice(data);
        SignedChange { change_type: ChangeType::AuthorizeActor, payload: Bytes::from(payload) }
    }

    /// A [`SignedAccountChanges`] batch whose `signature` is a fresh signature
    /// over its own digest (Local channel binds `LOCAL`).
    fn signed_change(
        account: Address,
        authenticator: Address,
        signer: &K256SigningKey,
        channel: AccountChangeChannel,
        sequence: u64,
        changes: Vec<SignedChange>,
    ) -> SignedAccountChanges {
        let mut change =
            SignedAccountChanges { channel, sequence, changes, signature: Bytes::new() };
        let digest = ConfigChangeAuthorizer::changes_digest(account, LOCAL, &change);
        change.signature = auth_blob(authenticator, &sig(signer, digest));
        change
    }

    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    #[test]
    fn implicit_eoa_owner_authorizes_config_change() {
        let k = key(0x11);
        let account = addr(&k);
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![revoke(0xab)]);
        with_storage(|acc| {
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert!(resolved.is_admin());
        });
    }

    #[test]
    fn configured_admin_actor_authorizes() {
        let k = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let id = actor_id(addr(&k));
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![revoke(0xcd)]);
        with_storage(|acc| {
            acc.actors
                .at_mut(&id)
                .at_mut(&account)
                .write(pack(K1, Eip8130Constants::SCOPE_UNRESTRICTED, 0))
                .unwrap();
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert!(resolved.is_admin());
        });
    }

    #[test]
    fn actor_without_config_scope_is_rejected() {
        let k = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let id = actor_id(addr(&k));
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![revoke(0x01)]);
        with_storage(|acc| {
            // Bound actor that lacks CONFIG (only SENDER).
            acc.actors
                .at_mut(&id)
                .at_mut(&account)
                .write(pack(K1, Eip8130Constants::SCOPE_OPERATOR, 0))
                .unwrap();
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::Scope {
                    operation: Operation::Config,
                    scope: Eip8130Constants::SCOPE_OPERATOR,
                }),
            );
        });
    }

    #[test]
    fn locked_account_batch_still_authorizes_signature() {
        let k = key(0x11);
        let account = addr(&k);
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Multichain, 0, vec![revoke(0x01)]);
        with_storage(|acc| {
            // Hard-locked (FLAG_LOCKED, no unlock initiated): frozen regardless of `now`.
            acc.account_state
                .at_mut(&account)
                .write(pack_state(0, 0, Eip8130Constants::FLAG_LOCKED, 0))
                .unwrap();
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert!(resolved.is_admin());
        });
    }

    #[test]
    fn stale_local_epoch_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        // Local batch committing epoch 0, but the account's local epoch is 2.
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Local, 0, vec![revoke(0x01)]);
        with_storage(|acc| {
            acc.account_state.at_mut(&account).write(pack_state_epoch(0, 0, 2, 0, 0)).unwrap();
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::StaleEpoch { expected: 2, got: 0 }),
            );
        });
    }

    #[test]
    fn stale_sequence_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        // Multichain channel sequence in state is 0; the batch claims 5.
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Multichain, 5, vec![revoke(0x01)]);
        with_storage(|acc| {
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::BadSequence { expected: 0, got: 5 }),
            );
        });
    }

    #[test]
    fn local_channel_uses_local_sequence() {
        let k = key(0x11);
        let account = addr(&k);
        // Local channel: the batch's low-half sequence must match local_sequence.
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Local, 3, vec![revoke(0x01)]);
        with_storage(|acc| {
            acc.account_state.at_mut(&account).write(pack_state(0, 3, 0, 0)).unwrap();
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert!(resolved.is_admin());
        });
    }

    #[test]
    fn unsequenced_local_batch_skips_sequence_check() {
        let k = key(0x11);
        let account = addr(&k);
        // Low half == UNSEQUENCED (JIT): consumes no counter, so it authorizes
        // regardless of the account's current local sequence.
        let sequence = u64::from(Eip8130Constants::UNSEQUENCED);
        let change =
            signed_change(account, K1, &k, AccountChangeChannel::Local, sequence, vec![revoke(1)]);
        with_storage(|acc| {
            acc.account_state.at_mut(&account).write(pack_state(0, 9, 0, 0)).unwrap();
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert!(resolved.is_admin());
        });
    }

    #[test]
    fn implicit_eoa_wrong_signer_is_rejected() {
        let owner = key(0x11);
        let account = addr(&owner);
        let attacker = key(0x99);
        let attacker_id = actor_id(addr(&attacker));
        // The digest binds `account`, but the signature is from a different key.
        let mut change = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![revoke(0x01)],
            signature: Bytes::new(),
        };
        let digest = ConfigChangeAuthorizer::changes_digest(account, LOCAL, &change);
        change.signature = auth_blob(K1, &sig(&attacker, digest));
        with_storage(|acc| {
            // The recovered signer is not the account and has no registered actor.
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::Authorize(AuthorizeError::AuthenticatorMismatch {
                    actor_id: attacker_id,
                    authenticator: Eip8130Constants::K1_AUTHENTICATOR,
                })),
            );
        });
    }

    #[test]
    fn digest_binds_account_channel_sequence_and_changes() {
        let account = address!("0x00000000000000000000000000000000000000aa");
        let base = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![revoke(0x01)],
            signature: Bytes::new(),
        };
        let d0 = ConfigChangeAuthorizer::changes_digest(account, LOCAL, &base);

        // Deterministic.
        assert_eq!(d0, ConfigChangeAuthorizer::changes_digest(account, LOCAL, &base));

        // Account, channel (chainId), sequence, and op content each shift the digest.
        let other = address!("0x00000000000000000000000000000000000000bb");
        assert_ne!(d0, ConfigChangeAuthorizer::changes_digest(other, LOCAL, &base));

        // Switching to Local binds `chainId = LOCAL` instead of 0.
        let mut channel = base.clone();
        channel.channel = AccountChangeChannel::Local;
        assert_ne!(d0, ConfigChangeAuthorizer::changes_digest(account, LOCAL, &channel));

        let mut seq = base.clone();
        seq.sequence = 1;
        assert_ne!(d0, ConfigChangeAuthorizer::changes_digest(account, LOCAL, &seq));

        let mut changed = base;
        changed.changes = vec![authorize_change(0x01, b"policy-data")];
        assert_ne!(d0, ConfigChangeAuthorizer::changes_digest(account, LOCAL, &changed));
    }
}
