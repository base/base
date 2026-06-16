//! Account-configuration change authorization — the `SCOPE_CONFIG` path of the
//! EIP-8130 validation flow.

use alloy_primitives::{Address, B256, Keccak256, b256, keccak256};
use base_common_consensus::ConfigChange;
use base_execution_eip8130_authorize::{ActorAuthorizer, AuthorizeError, ResolvedActor};
use base_execution_eip8130_state::AccountConfigurationStorage;

use crate::{Operation, TxAuthError};

/// Precomputed `keccak256` typehash of the `SignedActorChanges` EIP-712-style
/// struct, identical to the one hashed by `AccountConfiguration` (the trailing
/// `ActorChange(...)` is the referenced struct's type, per the EIP-712 encoding
/// rules):
/// `keccak256("SignedActorChanges(address account,uint64 chainId,uint64 sequence,ActorChange[] actorChanges)ActorChange(uint8 changeType,bytes32 actorId,bytes data)")`.
/// Pinned to its preimage by `typehashes_match_their_preimages`.
const SIGNED_ACTOR_CHANGES_TYPEHASH: B256 =
    b256!("3528344db25dddc3f16dbdc7302aacb555665c0af1beedc07d5fe28e8512bb3f");
/// Precomputed `keccak256` typehash for the per-change `ActorChange` leaves:
/// `keccak256("ActorChange(uint8 changeType,bytes32 actorId,bytes data)")`.
/// Pinned to its preimage by `typehashes_match_their_preimages`.
const ACTOR_CHANGE_TYPEHASH: B256 =
    b256!("76ddc97127f9b5fc6a394060571bb32201243ae2344f1f09e6039df7fd19bbd7");

/// Authorizes EIP-8130 account-configuration changes against an
/// [`AccountConfigurationStorage`] view.
///
/// Native mirror of `AccountConfiguration.applySignedActorChanges`'s
/// authorization tail: it reconstructs the `SignedActorChanges` digest, runs the
/// entry's `auth` through the stateful [`ActorAuthorizer`], and enforces the
/// `SCOPE_CONFIG` gate, the account lock, the chain binding, and the sequence
/// channel. It does **not** apply the actor changes (decode `data`, mutate
/// `actor_config`) — that is the consuming validator's responsibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfigChangeAuthorizer;

impl ConfigChangeAuthorizer {
    /// Authorize a single [`ConfigChange`] entry for `account` (the resolved
    /// transaction sender, against whose config the change applies).
    ///
    /// `now` is the timestamp used for the lock check and actor expiry (block
    /// timestamp at inclusion, wall-clock in the pool). Returns the authorizing
    /// actor's resolved surface, or the first [`TxAuthError`] encountered.
    ///
    /// Scope: authorizes exactly one entry against the account's *current*
    /// on-chain sequence. Ordering and sequence advancement across multiple
    /// same-channel entries within one transaction (and applying the changes) is
    /// the orchestrator's responsibility, layered on top.
    pub fn authorize(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        local_chain_id: u64,
        change: &ConfigChange,
        now: u64,
    ) -> Result<ResolvedActor, TxAuthError> {
        // Locked accounts reject all config changes (`onlyUnlocked`).
        if storage.is_locked(account, now).map_err(AuthorizeError::Storage)? {
            return Err(TxAuthError::AccountLocked);
        }

        // Chain binding + channel selection: 0 = multichain, else the local chain.
        if change.chain_id != 0 && change.chain_id != local_chain_id {
            return Err(TxAuthError::ConfigChainId {
                expected: local_chain_id,
                got: change.chain_id,
            });
        }

        // The contract reads the sequence from state and the signer signs over
        // the value that will be used; require the entry to match the account's
        // current channel sequence so the reconstructed digest is the signed one.
        let (multichain_seq, local_seq) =
            storage.get_change_sequences(account).map_err(AuthorizeError::Storage)?;
        let expected = if change.chain_id == 0 { multichain_seq } else { local_seq };
        if change.sequence != expected {
            return Err(TxAuthError::ConfigSequence { expected, got: change.sequence });
        }

        // Reconstruct the digest, authorize the entry's auth, and require CONFIG scope.
        let digest = Self::signed_actor_changes_digest(account, change);
        let resolved =
            ActorAuthorizer::authenticate_actor(storage, account, digest, &change.auth, now)?;
        if !Operation::Config.is_granted(&resolved) {
            return Err(TxAuthError::Scope { operation: Operation::Config, scope: resolved.scope });
        }
        Ok(resolved)
    }

    /// Computes the EIP-8130 `SignedActorChanges` digest for `change` against
    /// `account`, byte-identical to
    /// `AccountConfiguration._computeSignedActorChangesDigest`.
    ///
    /// Each actor change is hashed as
    /// `keccak256(abi.encode(ACTORCHANGE_TYPEHASH, changeType, actorId, keccak256(data)))`,
    /// the leaf hashes are concatenated (`abi.encodePacked`) and hashed, and the
    /// result is folded into the outer struct hash.
    #[must_use]
    pub fn signed_actor_changes_digest(account: Address, change: &ConfigChange) -> B256 {
        let mut packed = Keccak256::new();
        for ac in &change.actor_changes {
            // abi.encode(bytes32, uint8, bytes32, bytes32): four right-aligned words.
            let mut leaf = [0u8; 128];
            leaf[..32].copy_from_slice(ACTOR_CHANGE_TYPEHASH.as_slice());
            leaf[63] = ac.change_type.op_byte();
            leaf[64..96].copy_from_slice(ac.actor_id.as_slice());
            leaf[96..128].copy_from_slice(keccak256(&ac.data).as_slice());
            packed.update(keccak256(leaf).as_slice());
        }
        let actor_changes_hash = packed.finalize();

        // abi.encode(bytes32, address, uint64, uint64, bytes32): five right-aligned words.
        let mut outer = [0u8; 160];
        outer[..32].copy_from_slice(SIGNED_ACTOR_CHANGES_TYPEHASH.as_slice());
        outer[44..64].copy_from_slice(account.as_slice());
        outer[88..96].copy_from_slice(&change.chain_id.to_be_bytes());
        outer[120..128].copy_from_slice(&change.sequence.to_be_bytes());
        outer[128..160].copy_from_slice(actor_changes_hash.as_slice());
        keccak256(outer)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256, address};
    use base_common_consensus::{ActorChange, ActorChangeType, Eip8130Constants};
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;

    const NOW: u64 = 1_000;
    const LOCAL: u64 = 8453;
    const ECRECOVER: Address = Eip8130Constants::ECRECOVER_AUTHENTICATOR;

    #[test]
    fn typehashes_match_their_preimages() {
        assert_eq!(
            SIGNED_ACTOR_CHANGES_TYPEHASH,
            keccak256(
                b"SignedActorChanges(address account,uint64 chainId,uint64 sequence,ActorChange[] actorChanges)ActorChange(uint8 changeType,bytes32 actorId,bytes data)"
            )
        );
        assert_eq!(
            ACTOR_CHANGE_TYPEHASH,
            keccak256(b"ActorChange(uint8 changeType,bytes32 actorId,bytes data)")
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

    /// Canonical Solidity packing of `ActorConfig`.
    fn pack(authenticator: Address, scope: u8, expiry: u64, policy_type: u8) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(scope) << 160)
            | (U256::from(expiry) << 168)
            | (U256::from(policy_type) << 216)
    }

    /// Canonical Solidity packing of `AccountState`.
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

    fn authorize_change(actor_byte: u8, data: &[u8]) -> ActorChange {
        ActorChange {
            change_type: ActorChangeType::Authorize,
            actor_id: B256::repeat_byte(actor_byte),
            data: Bytes::copy_from_slice(data),
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

    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    #[test]
    fn implicit_eoa_owner_authorizes_config_change() {
        let k = key(0x11);
        let account = addr(&k);
        let change = signed_change(account, Address::ZERO, &k, 0, 0, vec![revoke(0xab)]);
        with_storage(|acc| {
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert!(resolved.is_unrestricted());
        });
    }

    #[test]
    fn configured_config_actor_authorizes() {
        let k = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let id = actor_id(addr(&k));
        let change = signed_change(account, ECRECOVER, &k, 0, 0, vec![revoke(0xcd)]);
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_CONFIG, 0, 0))
                .unwrap();
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert_eq!(resolved.scope, Eip8130Constants::SCOPE_CONFIG);
        });
    }

    #[test]
    fn actor_without_config_scope_is_rejected() {
        let k = key(0x22);
        let account = address!("0x00000000000000000000000000000000000000aa");
        let id = actor_id(addr(&k));
        let change = signed_change(account, ECRECOVER, &k, 0, 0, vec![revoke(0x01)]);
        with_storage(|acc| {
            // Bound actor that lacks CONFIG (only SENDER).
            acc.actor_config
                .at_mut(&id)
                .at_mut(&account)
                .write(pack(ECRECOVER, Eip8130Constants::SCOPE_SENDER, 0, 0))
                .unwrap();
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::Scope {
                    operation: Operation::Config,
                    scope: Eip8130Constants::SCOPE_SENDER,
                }),
            );
        });
    }

    #[test]
    fn locked_account_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        let change = signed_change(account, Address::ZERO, &k, 0, 0, vec![revoke(0x01)]);
        with_storage(|acc| {
            // Locked until after `now`.
            acc.account_state.at_mut(&account).write(pack_state(0, 0, NOW + 1)).unwrap();
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::AccountLocked),
            );
        });
    }

    #[test]
    fn foreign_chain_id_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        let change = signed_change(account, Address::ZERO, &k, LOCAL + 1, 0, vec![revoke(0x01)]);
        with_storage(|acc| {
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::ConfigChainId { expected: LOCAL, got: LOCAL + 1 }),
            );
        });
    }

    #[test]
    fn stale_sequence_is_rejected() {
        let k = key(0x11);
        let account = addr(&k);
        // Multichain channel sequence in state is 0; the entry claims 5.
        let change = signed_change(account, Address::ZERO, &k, 0, 5, vec![revoke(0x01)]);
        with_storage(|acc| {
            assert_eq!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::ConfigSequence { expected: 0, got: 5 }),
            );
        });
    }

    #[test]
    fn local_channel_uses_local_sequence() {
        let k = key(0x11);
        let account = addr(&k);
        // Local channel (chain_id == LOCAL); the entry must match local_sequence.
        let change = signed_change(account, Address::ZERO, &k, LOCAL, 3, vec![revoke(0x01)]);
        with_storage(|acc| {
            acc.account_state.at_mut(&account).write(pack_state(0, 3, 0)).unwrap();
            let resolved =
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW).unwrap();
            assert!(resolved.is_unrestricted());
        });
    }

    #[test]
    fn implicit_eoa_wrong_signer_is_rejected() {
        let owner = key(0x11);
        let account = addr(&owner);
        let attacker = key(0x99);
        // The digest binds `account`, but the auth is signed by a different key.
        let mut change = ConfigChange {
            chain_id: 0,
            sequence: 0,
            actor_changes: vec![revoke(0x01)],
            auth: Bytes::new(),
        };
        let digest = ConfigChangeAuthorizer::signed_actor_changes_digest(account, &change);
        change.auth = auth_blob(Address::ZERO, &sig(&attacker, digest));
        with_storage(|acc| {
            assert!(matches!(
                ConfigChangeAuthorizer::authorize(acc, account, LOCAL, &change, NOW),
                Err(TxAuthError::Authorize(AuthorizeError::ImplicitEoaMismatch)),
            ));
        });
    }

    #[test]
    fn digest_binds_account_chain_sequence_and_changes() {
        let account = address!("0x00000000000000000000000000000000000000aa");
        let base = ConfigChange {
            chain_id: 0,
            sequence: 0,
            actor_changes: vec![revoke(0x01)],
            auth: Bytes::new(),
        };
        let d0 = ConfigChangeAuthorizer::signed_actor_changes_digest(account, &base);

        // Deterministic.
        assert_eq!(d0, ConfigChangeAuthorizer::signed_actor_changes_digest(account, &base));

        // Account, chain, sequence, and actor-change content each shift the digest.
        let other = address!("0x00000000000000000000000000000000000000bb");
        assert_ne!(d0, ConfigChangeAuthorizer::signed_actor_changes_digest(other, &base));

        let mut chain = base.clone();
        chain.chain_id = LOCAL;
        assert_ne!(d0, ConfigChangeAuthorizer::signed_actor_changes_digest(account, &chain));

        let mut seq = base.clone();
        seq.sequence = 1;
        assert_ne!(d0, ConfigChangeAuthorizer::signed_actor_changes_digest(account, &seq));

        let mut changed = base;
        changed.actor_changes = vec![authorize_change(0x01, b"policy-data")];
        assert_ne!(d0, ConfigChangeAuthorizer::signed_actor_changes_digest(account, &changed));
    }
}
