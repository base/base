//! Stateful EIP-8130 actor authorization: resolve an auth blob to an authorized
//! actor against the `AccountConfiguration` storage.
//!
//! Native mirror of `AccountConfiguration.authenticateActor` / `_authenticate`.

use alloy_primitives::{Address, B256};
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};

use crate::{
    AccountConfigurationStorage, AuthError, AuthenticatorDispatch, AuthorizeError, DispatchOutcome,
    RecoveredActorId, ResolvedActor,
};

/// Authorizes actors against an [`AccountConfigurationStorage`] view.
///
/// Stateless of the EVM otherwise: all account state flows through the storage
/// reader, so the same logic runs over the EVM journal (inclusion), a
/// `StateProvider` adapter (mempool), or an in-memory map (tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActorAuthorizer;

impl ActorAuthorizer {
    /// Authenticate and authorize `auth` (`authenticator(20) || data`) for
    /// `account` against `hash`, returning the resolved actor's authorization
    /// surface. `now` is the timestamp used for expiry (block timestamp at
    /// inclusion, wall-clock in the pool).
    ///
    /// Mirrors `AccountConfiguration.authenticateActor`: the contract reverts on
    /// any failure; here that maps to an [`AuthorizeError`].
    pub fn authenticate_actor(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        hash: B256,
        auth: &[u8],
        now: u64,
    ) -> Result<ResolvedActor, AuthorizeError> {
        if auth.len() < 20 {
            return Err(AuthError::MalformedAuth.into());
        }
        let authenticator = Address::from_slice(&auth[..20]);
        Self::authenticate(storage, account, hash, authenticator, &auth[20..], now)
    }

    /// Mirror of `_authenticate`: route by authenticator, then authorize the
    /// resolved actor against the account's config.
    fn authenticate(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        hash: B256,
        authenticator: Address,
        data: &[u8],
        now: u64,
    ) -> Result<ResolvedActor, AuthorizeError> {
        // secp256k1 signers — the implicit default EOA and every explicit k1
        // actor — authenticate through `K1_AUTHENTICATOR`. Recover here (the
        // `RecoveredActorId` token proves the recovery) and authorize against
        // the account.
        if authenticator == Eip8130Constants::K1_AUTHENTICATOR {
            return Self::authorize_k1(
                storage,
                account,
                RecoveredActorId::recover_k1(hash, data)?,
                now,
            );
        }

        // P-256, WebAuthn, and delegate route through enshrined dispatch;
        // non-canonical authenticators are rejected there.
        match AuthenticatorDispatch::authenticate(hash, authenticator, data)? {
            DispatchOutcome::Authenticated { actor_id } => {
                Self::resolve_bound(storage, account, actor_id, authenticator, now)
            }
            DispatchOutcome::Delegated { actor_id, delegate_account } => {
                // `data` = delegate_account(20) || nested_auth. Mirror
                // `DelegateAuthenticator`, which calls
                // `authenticateActor(delegate, hash, nestedAuth)` — the *full*
                // auth path (inline default-EOA k1 self *or* explicit
                // `actor_config`), then requires admin (`scope == 0`). Nested
                // discharge must not skip to `resolve_bound`: that would reject a
                // live default EOA whose key lives only in `AccountState`, the
                // common EOA-as-parent case. This is also the single nested
                // signature verification (dispatch's delegate step is structural
                // only), so there is no redundant ecrecover. The admin gate is
                // independent of `verifySignature` (now operational: admin, or a
                // SENDER actor without POLICY): an operational key may sign for
                // its own account but MUST NOT vouch as a delegate, to preserve
                // non-escalation. Followed by the outer
                // `_actorConfig[bytes20(delegate)][account]` binding check.
                //
                // Independent depth-1 guard: `authenticate_actor` re-enters the
                // public dispatch, which routes a delegate authenticator straight
                // to the (structural) delegate step, so reject a nested delegate
                // here before re-entry. `AuthenticatorDispatch::delegate` already
                // enforces this structurally; this second, layer-local check keeps
                // single-hop intact even if either layer is later refactored.
                // (`data` is `delegate_account(20) || nested_auth`, so
                // `data[20..40]` is the nested authenticator; the outer dispatch
                // guarantees `data.len() >= 40`.)
                let nested_authenticator = Address::from_slice(&data[20..40]);
                if nested_authenticator == Eip8130Contracts::DELEGATE_AUTHENTICATOR {
                    return Err(AuthError::NestedDelegate.into());
                }
                let nested =
                    Self::authenticate_actor(storage, delegate_account, hash, &data[20..], now)?;
                if nested.scope != 0 {
                    return Err(AuthorizeError::NestedSignatureScope { actor_id: nested.actor_id });
                }
                Self::resolve_bound(
                    storage,
                    account,
                    actor_id,
                    Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                    now,
                )
            }
        }
    }

    /// Mirror of `_authenticateK1` after recovery: resolve a recovered secp256k1
    /// signer against `account`.
    ///
    /// The account's own key — the **secp256k1 self** (`recovered ==
    /// bytes32(bytes20(account))`) — resolves entirely from the inline config in
    /// the account-state slot, a single SLOAD: a set `DEFAULT_EOA_REVOKED` flag
    /// disables it (revoked, or a non-k1 self is the live self authenticator), an
    /// all-zero inline config is the implicit full owner, and a non-zero inline
    /// `scope`/`expiry` is a scoped self. Every *other* recovered
    /// signer must carry an explicit k1 `actor_config` entry, validated by
    /// [`Self::resolve_bound`].
    ///
    /// `recovered` is a [`RecoveredActorId`] — a proof-of-recovery token, so
    /// this method trusts it as a genuinely recovered signer without
    /// re-verifying. The token can only be produced by a recovery constructor
    /// ([`RecoveredActorId::recover_k1`] / [`RecoveredActorId::recover_eoa_sender`]),
    /// which keeps this `pub` entrypoint from granting owner access on a bare
    /// caller-supplied `B256`. The empty-`sender` transaction path recovers the
    /// signer once in the verifier and passes the token here rather than
    /// re-recovering.
    pub fn authorize_k1(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        recovered: RecoveredActorId,
        now: u64,
    ) -> Result<ResolvedActor, AuthorizeError> {
        let recovered = recovered.actor_id();
        if recovered == AccountConfigurationStorage::self_actor_id(account) {
            let state = storage.get_account_state(account)?;
            // Flag set => the inline k1 self is disabled: either revoked outright
            // or superseded by a non-k1 self in `actor_config`. A k1 signature
            // recovering to the account can never authorize in that state.
            if state.default_eoa_revoked() {
                return Err(AuthorizeError::DefaultEoaRevoked { account });
            }
            // 0 = no expiry; otherwise valid while now <= expiry.
            if state.default_eoa_expiry != 0 && now > state.default_eoa_expiry {
                return Err(AuthorizeError::Expired {
                    actor_id: recovered,
                    expiry: state.default_eoa_expiry,
                });
            }
            // `_resolvePolicyTarget`: address(0) when ungated, else the policy
            // manager (keyed by the self-actorId, shared keyspace). An ungated
            // (full-owner) self costs no extra read.
            let policy_target = if state.default_eoa_scope & Eip8130Constants::SCOPE_POLICY == 0 {
                Address::ZERO
            } else {
                storage.get_policy_manager(account, recovered)?
            };
            return Ok(ResolvedActor {
                actor_id: recovered,
                scope: state.default_eoa_scope,
                policy_target,
            });
        }
        Self::resolve_bound(storage, account, recovered, Eip8130Constants::K1_AUTHENTICATOR, now)
    }

    /// Loads `actor_config[actor_id][account]`, requires it to be bound to
    /// `authenticator` and not expired, and returns the authorization surface
    /// (`scope`, resolved `policy_target`). Mirrors the shared tail
    /// of `_authenticate` / `_authenticateK1`.
    fn resolve_bound(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        authenticator: Address,
        now: u64,
    ) -> Result<ResolvedActor, AuthorizeError> {
        if actor_id.is_zero() {
            return Err(AuthorizeError::ZeroActor);
        }
        let config = storage.get_actor_config(account, actor_id)?;
        if config.authenticator != authenticator {
            return Err(AuthorizeError::NotBound { actor_id, authenticator });
        }
        // 0 = no expiry; otherwise valid while now <= expiry.
        if config.expiry != 0 && now > config.expiry {
            return Err(AuthorizeError::Expired { actor_id, expiry: config.expiry });
        }
        // `_resolvePolicyTarget`: address(0) when ungated, else the policy manager
        // (never the signed commitment). Resolved from the `config` already in
        // hand so an ungated actor costs no extra read and a gated one reads only
        // the manager slot (no `actor_config` re-read).
        let policy_target = if config.scope & Eip8130Constants::SCOPE_POLICY == 0 {
            Address::ZERO
        } else {
            storage.get_policy_manager(account, actor_id)?
        };
        Ok(ResolvedActor { actor_id, scope: config.scope, policy_target })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address, keccak256};
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey as K256SigningKey;
    use p256::ecdsa::{
        Signature as P256Sig, SigningKey as P256SigningKey, signature::hazmat::PrehashSigner,
    };

    use super::*;

    const HASH: B256 = B256::repeat_byte(0x42);
    const NOW: u64 = 1_000;
    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000a1");

    /// Canonical Solidity packing of `ActorConfig` (each field at its bit offset).
    fn pack(authenticator: Address, scope: u8, expiry: u64) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(scope) << 160)
            | (U256::from(expiry) << 168)
    }

    /// Packs an `AccountState` word carrying the inline secp256k1 self config
    /// (each field at its bit offset; sequences/lock left zero).
    fn pack_self(scope: u8, expiry: u64, revoked: bool) -> U256 {
        let flags = if revoked { Eip8130Constants::DEFAULT_EOA_REVOKED } else { 0 };
        (U256::from(flags) << 128) | (U256::from(scope) << 176) | (U256::from(expiry) << 184)
    }

    fn actor_id(address: Address) -> B256 {
        AccountConfigurationStorage::self_actor_id(address)
    }

    fn k1_key(byte: u8) -> K256SigningKey {
        K256SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    fn k1_address(key: &K256SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    /// 65-byte `r || s || v` signature over `hash`, `v` in `{27, 28}`.
    fn k1_sig(key: &K256SigningKey, hash: B256) -> [u8; 65] {
        let (sig, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    fn p256_key(byte: u8) -> P256SigningKey {
        P256SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    /// `data = r || s || x || y || pre_hash` for the P-256 authenticator, plus the
    /// derived `actorId = keccak256(x || y)`.
    fn p256_blob(key: &P256SigningKey, hash: B256) -> (Vec<u8>, B256) {
        let point = key.verifying_key().to_encoded_point(false);
        let bytes = point.as_bytes();
        let (x, y) = (&bytes[1..33], &bytes[33..65]);
        let sig: P256Sig = key.sign_prehash(hash.as_slice()).unwrap();
        let sig = sig.normalize_s().unwrap_or(sig);
        let mut data = Vec::with_capacity(129);
        data.extend_from_slice(&sig.to_bytes());
        data.extend_from_slice(x);
        data.extend_from_slice(y);
        data.push(0);
        (data, keccak256([x, y].concat()))
    }

    /// `authenticator(20) || data`.
    fn blob(authenticator: Address, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + data.len());
        out.extend_from_slice(authenticator.as_slice());
        out.extend_from_slice(data);
        out
    }

    /// Runs `body` with a fresh in-memory `AccountConfigurationStorage`.
    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    #[test]
    fn implicit_eoa_authorizes_unrestricted_owner() {
        let key = k1_key(0x11);
        let account = k1_address(&key);
        // The k1 blob whose signer recovers to the account: a live default EOA
        // (flag unset, no explicit self entry) resolves to the unrestricted owner.
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            let resolved = ActorAuthorizer::authenticate_actor(acc, account, HASH, &auth, NOW);
            assert_eq!(resolved.unwrap(), ResolvedActor::unrestricted(actor_id(account)));
        });
    }

    #[test]
    fn default_eoa_revoked_self_is_rejected() {
        let key = k1_key(0x11);
        let account = k1_address(&key);
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            // DEFAULT_EOA_REVOKED set: the inline k1 self is disabled (revoked, or a
            // non-k1 self is the live self authenticator), so a k1 signature
            // recovering to the account is rejected outright.
            acc.account_state.at_mut(&account).write(pack_self(0, 0, true)).unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, account, HASH, &auth, NOW),
                Err(AuthorizeError::DefaultEoaRevoked { account }),
            );
        });
    }

    #[test]
    fn scoped_self_resolves_inline_config() {
        let key = k1_key(0x11);
        let account = k1_address(&key);
        let self_id = actor_id(account);
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            // Flag unset with an inline scope: the self key is live but scoped, and
            // resolves from the account-state slot alone (no `actor_config` read).
            acc.account_state
                .at_mut(&account)
                .write(pack_self(Eip8130Constants::SCOPE_SENDER, 0, false))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, account, HASH, &auth, NOW).unwrap();
            assert_eq!(resolved.actor_id, self_id);
            assert_eq!(resolved.scope, Eip8130Constants::SCOPE_SENDER);
            assert_eq!(resolved.policy_target, Address::ZERO);
        });
    }

    #[test]
    fn expired_self_is_rejected() {
        let key = k1_key(0x11);
        let account = k1_address(&key);
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            // Inline expiry in the past: the self key is no longer valid.
            acc.account_state.at_mut(&account).write(pack_self(0, NOW - 1, false)).unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, account, HASH, &auth, NOW),
                Err(AuthorizeError::Expired { actor_id: actor_id(account), expiry: NOW - 1 }),
            );
        });
    }

    #[test]
    fn gated_self_resolves_inline_policy_manager() {
        let key = k1_key(0x11);
        let account = k1_address(&key);
        let self_id = actor_id(account);
        let manager = address!("0x00000000000000000000000000000000000000d4");
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            // Inline SCOPE_POLICY set: the self key is gated and resolves its policy
            // target from `policy_manager[self][account]`.
            acc.account_state
                .at_mut(&account)
                .write(pack_self(Eip8130Constants::SCOPE_POLICY, 0, false))
                .unwrap();
            acc.policy_manager.at_mut(&self_id).at_mut(&account).write(manager).unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, account, HASH, &auth, NOW).unwrap();
            assert_eq!(resolved.actor_id, self_id);
            assert!(resolved.is_policy_gated());
            assert_eq!(resolved.policy_target, manager);
        });
    }

    #[test]
    fn k1_signer_without_actor_entry_is_rejected() {
        let key = k1_key(0x11);
        // Signer recovers to a non-account address with no registered actor entry.
        let id = actor_id(k1_address(&key));
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NotBound {
                    actor_id: id,
                    authenticator: Eip8130Constants::K1_AUTHENTICATOR,
                }),
            );
        });
    }

    #[test]
    fn explicit_k1_resolves_bound_actor_surface() {
        let key = k1_key(0x22);
        let id = actor_id(k1_address(&key));
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Constants::K1_AUTHENTICATOR, 0x04, 0))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(
                resolved,
                ResolvedActor { actor_id: id, scope: 0x04, policy_target: Address::ZERO }
            );
        });
    }

    #[test]
    fn ecrecover_unbound_actor_is_rejected() {
        let key = k1_key(0x22);
        let id = actor_id(k1_address(&key));
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NotBound {
                    actor_id: id,
                    authenticator: Eip8130Constants::K1_AUTHENTICATOR,
                }),
            );
        });
    }

    #[test]
    fn expiry_is_enforced_against_now() {
        let key = k1_key(0x22);
        let id = actor_id(k1_address(&key));
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Constants::K1_AUTHENTICATOR, 0, 500))
                .unwrap();
            // Valid at/under expiry.
            assert!(ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, 500).is_ok());
            // Expired once now > expiry.
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, 501),
                Err(AuthorizeError::Expired { actor_id: id, expiry: 500 }),
            );
        });
    }

    #[test]
    fn gated_actor_resolves_policy_manager_target() {
        let key = k1_key(0x22);
        let id = actor_id(k1_address(&key));
        let manager = address!("0x00000000000000000000000000000000000000d4");
        let auth = blob(Eip8130Constants::K1_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Constants::K1_AUTHENTICATOR, Eip8130Constants::SCOPE_POLICY, 0))
                .unwrap();
            acc.policy_manager.at_mut(&id).at_mut(&ACCOUNT).write(manager).unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert!(resolved.is_policy_gated());
            assert_eq!(resolved.policy_target, manager);
        });
    }

    #[test]
    fn p256_resolves_keccak_xy_actor() {
        let key = p256_key(0x33);
        let (data, id) = p256_blob(&key, HASH);
        let auth = blob(Eip8130Contracts::P256_AUTHENTICATOR, &data);
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Contracts::P256_AUTHENTICATOR, 0x02, 0))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(
                resolved,
                ResolvedActor { actor_id: id, scope: 0x02, policy_target: Address::ZERO }
            );
        });
    }

    /// `DELEGATE || delegate_account(20) || K1_AUTHENTICATOR || nested_sig`.
    fn delegate_auth(delegate_account: Address, nested_key: &K256SigningKey) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(delegate_account.as_slice());
        data.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        data.extend_from_slice(&k1_sig(nested_key, HASH));
        blob(Eip8130Contracts::DELEGATE_AUTHENTICATOR, &data)
    }

    #[test]
    fn delegate_authorizes_nested_then_outer_surface() {
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");
        let nested_key = k1_key(0x44);
        let nested_id = actor_id(k1_address(&nested_key));
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            // Nested actor authorized on the delegated account.
            acc.actor_config
                .at_mut(&nested_id)
                .at_mut(&delegate_account)
                .write(pack(Eip8130Constants::K1_AUTHENTICATOR, 0, 0))
                .unwrap();
            // Outer delegate actor on the originating account carries the surface.
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Contracts::DELEGATE_AUTHENTICATOR, 0x08, 0))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(
                resolved,
                ResolvedActor { actor_id: outer_id, scope: 0x08, policy_target: Address::ZERO }
            );
        });
    }

    #[test]
    fn delegate_rejects_nested_actor_without_signature_scope() {
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");
        let nested_key = k1_key(0x44);
        let nested_id = actor_id(k1_address(&nested_key));
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            // Nested actor is bound on B but scoped (non-admin), so the delegate
            // vouch — which `DelegateAuthenticator` requires to be admin
            // (`scope == 0`) — rejects it.
            acc.actor_config
                .at_mut(&nested_id)
                .at_mut(&delegate_account)
                .write(pack(
                    Eip8130Constants::K1_AUTHENTICATOR,
                    Eip8130Constants::SCOPE_SELF_PAYER,
                    0,
                ))
                .unwrap();
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(
                    Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                    Eip8130Constants::SCOPE_SENDER,
                    0,
                ))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NestedSignatureScope { actor_id: nested_id }),
            );
        });
    }

    #[test]
    fn delegate_accepts_nested_actor_with_signature_scope() {
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");
        let nested_key = k1_key(0x44);
        let nested_id = actor_id(k1_address(&nested_key));
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            // Nested actor is admin (`scope == 0`), the predicate the delegate
            // vouch requires, so it satisfies the delegate gate.
            acc.actor_config
                .at_mut(&nested_id)
                .at_mut(&delegate_account)
                .write(pack(Eip8130Constants::K1_AUTHENTICATOR, 0, 0))
                .unwrap();
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(
                    Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                    Eip8130Constants::SCOPE_SENDER,
                    0,
                ))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(resolved.actor_id, outer_id);
            assert_eq!(resolved.scope, Eip8130Constants::SCOPE_SENDER);
        });
    }

    #[test]
    fn delegate_rejects_unbound_nested_actor() {
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");
        let nested_key = k1_key(0x44);
        let nested_id = actor_id(k1_address(&nested_key));
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            // Only the outer actor is registered; the nested actor is not bound on B.
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Contracts::DELEGATE_AUTHENTICATOR, 0x08, 0))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NotBound {
                    actor_id: nested_id,
                    authenticator: Eip8130Constants::K1_AUTHENTICATOR,
                }),
            );
        });
    }

    #[test]
    fn nested_delegate_is_rejected() {
        // Depth-2: DELEGATE || delegate_account(20) || DELEGATE || .... Single-hop
        // is rejected. On the public path the dispatch-level structural check
        // (`AuthenticatorDispatch::delegate`) fires first, so that is what this
        // test exercises; the authorize-layer guard is redundant defense-in-depth
        // that only becomes reachable if the dispatch check were removed. Both
        // surface the same `NestedDelegate` error.
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");
        let mut data = Vec::new();
        data.extend_from_slice(delegate_account.as_slice());
        data.extend_from_slice(Eip8130Contracts::DELEGATE_AUTHENTICATOR.as_slice());
        data.extend_from_slice(&[0u8; 65]);
        let auth = blob(Eip8130Contracts::DELEGATE_AUTHENTICATOR, &data);
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::Authenticate(AuthError::NestedDelegate)),
            );
        });
    }

    #[test]
    fn delegate_accepts_nested_default_eoa_self() {
        // EOA as parent: nested k1 recovers to the delegate account itself,
        // with no `actor_config` entry — only the live inline default EOA.
        // `DelegateAuthenticator` → `authenticateActor` must honor that path;
        // bare `resolve_bound` would incorrectly return NotBound.
        let nested_key = k1_key(0x55);
        let delegate_account = k1_address(&nested_key);
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Contracts::DELEGATE_AUTHENTICATOR, 0x08, 0))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(
                resolved,
                ResolvedActor { actor_id: outer_id, scope: 0x08, policy_target: Address::ZERO }
            );
        });
    }

    #[test]
    fn delegate_rejects_nested_default_eoa_when_revoked() {
        let nested_key = k1_key(0x56);
        let delegate_account = k1_address(&nested_key);
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            acc.account_state.at_mut(&delegate_account).write(pack_self(0, 0, true)).unwrap();
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Contracts::DELEGATE_AUTHENTICATOR, 0x08, 0))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::DefaultEoaRevoked { account: delegate_account }),
            );
        });
    }

    #[test]
    fn delegate_rejects_nested_scoped_default_eoa() {
        // Live but scoped (non-admin) default EOA may sign for its own account,
        // yet must not vouch as a delegate.
        let nested_key = k1_key(0x57);
        let delegate_account = k1_address(&nested_key);
        let nested_id = actor_id(delegate_account);
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            acc.account_state
                .at_mut(&delegate_account)
                .write(pack_self(Eip8130Constants::SCOPE_SENDER, 0, false))
                .unwrap();
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Contracts::DELEGATE_AUTHENTICATOR, 0x08, 0))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NestedSignatureScope { actor_id: nested_id }),
            );
        });
    }

    #[test]
    fn delegate_rejects_unbound_outer_actor() {
        let delegate_account = address!("0x00000000000000000000000000000000000000bb");
        let nested_key = k1_key(0x44);
        let nested_id = actor_id(k1_address(&nested_key));
        let outer_id = actor_id(delegate_account);
        let auth = delegate_auth(delegate_account, &nested_key);
        with_storage(|acc| {
            // Nested is bound, but the outer delegate actor is missing on A.
            acc.actor_config
                .at_mut(&nested_id)
                .at_mut(&delegate_account)
                .write(pack(Eip8130Constants::K1_AUTHENTICATOR, 0, 0))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NotBound {
                    actor_id: outer_id,
                    authenticator: Eip8130Contracts::DELEGATE_AUTHENTICATOR,
                }),
            );
        });
    }

    #[test]
    fn delegate_to_zero_address_is_rejected_as_zero_actor() {
        // A delegate to address(0) yields an outer actor id of bytes32(0).
        let nested_key = k1_key(0x44);
        let nested_id = actor_id(k1_address(&nested_key));
        let auth = delegate_auth(Address::ZERO, &nested_key);
        with_storage(|acc| {
            // Bind the nested actor on address(0) so the nested discharge passes
            // and we reach the outer zero-actor guard.
            acc.actor_config
                .at_mut(&nested_id)
                .at_mut(&Address::ZERO)
                .write(pack(Eip8130Constants::K1_AUTHENTICATOR, 0, 0))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::ZeroActor),
            );
        });
    }

    #[test]
    fn auth_shorter_than_an_authenticator_is_malformed() {
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &[0u8; 10], NOW),
                Err(AuthorizeError::Authenticate(AuthError::MalformedAuth)),
            );
        });
    }

    #[test]
    fn zero_authenticator_selector_is_rejected() {
        // `address(0)` is the empty sentinel, never a valid authenticator selector.
        let auth = blob(Address::ZERO, &[0u8; 65]);
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::Authenticate(AuthError::NotCanonical(Address::ZERO))),
            );
        });
    }

    #[test]
    fn non_canonical_authenticator_is_rejected() {
        let authenticator = address!("0x00000000000000000000000000000000deadbeef");
        let auth = blob(authenticator, &[0u8; 65]);
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::Authenticate(AuthError::NotCanonical(authenticator))),
            );
        });
    }
}
