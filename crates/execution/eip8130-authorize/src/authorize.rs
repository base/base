//! Stateful EIP-8130 actor authorization: resolve an auth blob to an authorized
//! actor against the `AccountConfiguration` storage.
//!
//! Native mirror of `AccountConfiguration.authenticateActor` / `_authenticate`.

use alloy_primitives::{Address, B256};
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};
use base_execution_eip8130::{AuthError, AuthenticatorDispatch, DispatchOutcome};
use base_execution_eip8130_state::AccountConfigurationStorage;

use crate::{AuthorizeError, ResolvedActor};

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
        // `address(0)` is the implicit-EOA owner: it needs state (empty self-slot
        // + recovered == account) and so is handled here, not by stateless dispatch.
        if authenticator == Address::ZERO {
            return Self::authenticate_implicit_eoa(storage, account, hash, data);
        }

        // The native ecrecover sentinel, P-256, WebAuthn, and delegate all route
        // through enshrined dispatch; REVOKED and non-canonical are rejected there.
        match AuthenticatorDispatch::authenticate(hash, authenticator, data)? {
            DispatchOutcome::Authenticated { actor_id } => {
                Self::resolve_bound(storage, account, actor_id, authenticator, now)
            }
            DispatchOutcome::Delegated {
                actor_id,
                delegate_account,
                nested_authenticator,
                nested_actor_id,
            } => {
                // Discharge the nested actor against the delegated account's
                // config and require it to carry SIGNATURE scope, then authorize
                // the outer delegate actor against the originating account.
                // Mirrors `DelegateAuthenticator` calling
                // `verifySignature(delegate, ...)` (which accepts only an
                // unrestricted or `SCOPE_SIGNATURE` actor) followed by the outer
                // `_actorConfig[bytes20(delegate)][account]` binding check.
                let nested = Self::resolve_bound(
                    storage,
                    delegate_account,
                    nested_actor_id,
                    nested_authenticator,
                    now,
                )?;
                if nested.scope != 0 && nested.scope & Eip8130Constants::SCOPE_SIGNATURE == 0 {
                    return Err(AuthorizeError::NestedSignatureScope { actor_id: nested_actor_id });
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

    /// Mirror of `_authenticateImplicitEOA`: native ecrecover, requires the
    /// self-actor slot empty and `recovered == account`. Implicit EOAs are always
    /// unrestricted owners with no policy.
    fn authenticate_implicit_eoa(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        hash: B256,
        data: &[u8],
    ) -> Result<ResolvedActor, AuthorizeError> {
        let self_id = AccountConfigurationStorage::self_actor_id(account);
        if !storage.get_actor_config(account, self_id)?.is_empty() {
            return Err(AuthorizeError::ImplicitEoaShadowed);
        }
        // Reuse the enshrined ecrecover (identical to the sentinel path).
        let recovered = match AuthenticatorDispatch::authenticate(
            hash,
            Eip8130Constants::ECRECOVER_AUTHENTICATOR,
            data,
        )? {
            DispatchOutcome::Authenticated { actor_id } => actor_id,
            // ecrecover never produces a delegate obligation.
            DispatchOutcome::Delegated { .. } => return Err(AuthError::InvalidSignature.into()),
        };
        if recovered != self_id {
            return Err(AuthorizeError::ImplicitEoaMismatch);
        }
        Ok(ResolvedActor::unrestricted(self_id))
    }

    /// Loads `actor_config[actor_id][account]`, requires it to be bound to
    /// `authenticator` and not expired, and returns the authorization surface
    /// (`scope`, `policy_type`, resolved `policy_target`). Mirrors the shared tail
    /// of `_authenticate` / `_authenticateEcrecover`.
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
        let policy_target = if config.policy_type == 0 {
            Address::ZERO
        } else {
            storage.get_policy_manager(account, actor_id)?
        };
        Ok(ResolvedActor {
            actor_id,
            scope: config.scope,
            policy_type: config.policy_type,
            policy_target,
        })
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
    fn pack(authenticator: Address, scope: u8, expiry: u64, policy_type: u8) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(scope) << 160)
            | (U256::from(expiry) << 168)
            | (U256::from(policy_type) << 216)
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
        let auth = blob(Address::ZERO, &k1_sig(&key, HASH));
        with_storage(|acc| {
            let resolved = ActorAuthorizer::authenticate_actor(acc, account, HASH, &auth, NOW);
            assert_eq!(resolved.unwrap(), ResolvedActor::unrestricted(actor_id(account)));
        });
    }

    #[test]
    fn implicit_eoa_rejected_when_self_slot_occupied() {
        let key = k1_key(0x11);
        let account = k1_address(&key);
        let self_id = actor_id(account);
        let auth = blob(Address::ZERO, &k1_sig(&key, HASH));
        with_storage(|acc| {
            // An explicit actor registered at the self id shadows the implicit owner.
            acc.actor_config
                .at_mut(&self_id)
                .at_mut(&account)
                .write(pack(Eip8130Constants::ECRECOVER_AUTHENTICATOR, 0, 0, 0))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, account, HASH, &auth, NOW),
                Err(AuthorizeError::ImplicitEoaShadowed),
            );
        });
    }

    #[test]
    fn implicit_eoa_rejected_when_signer_is_not_the_account() {
        let key = k1_key(0x11);
        // Authenticate against a different account than the signer recovers to.
        let auth = blob(Address::ZERO, &k1_sig(&key, HASH));
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::ImplicitEoaMismatch),
            );
        });
    }

    #[test]
    fn explicit_ecrecover_resolves_bound_actor_surface() {
        let key = k1_key(0x22);
        let id = actor_id(k1_address(&key));
        let auth = blob(Eip8130Constants::ECRECOVER_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Constants::ECRECOVER_AUTHENTICATOR, 0x04, 0, 0))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(
                resolved,
                ResolvedActor {
                    actor_id: id,
                    scope: 0x04,
                    policy_type: 0,
                    policy_target: Address::ZERO
                }
            );
        });
    }

    #[test]
    fn ecrecover_unbound_actor_is_rejected() {
        let key = k1_key(0x22);
        let id = actor_id(k1_address(&key));
        let auth = blob(Eip8130Constants::ECRECOVER_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NotBound {
                    actor_id: id,
                    authenticator: Eip8130Constants::ECRECOVER_AUTHENTICATOR,
                }),
            );
        });
    }

    #[test]
    fn expiry_is_enforced_against_now() {
        let key = k1_key(0x22);
        let id = actor_id(k1_address(&key));
        let auth = blob(Eip8130Constants::ECRECOVER_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Constants::ECRECOVER_AUTHENTICATOR, 0, 500, 0))
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
        let auth = blob(Eip8130Constants::ECRECOVER_AUTHENTICATOR, &k1_sig(&key, HASH));
        with_storage(|acc| {
            acc.actor_config
                .at_mut(&id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Constants::ECRECOVER_AUTHENTICATOR, 0, 0, 1))
                .unwrap();
            acc.policy_manager.at_mut(&id).at_mut(&ACCOUNT).write(manager).unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(resolved.policy_type, 1);
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
                .write(pack(Eip8130Contracts::P256_AUTHENTICATOR, 0x02, 0, 0))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(
                resolved,
                ResolvedActor {
                    actor_id: id,
                    scope: 0x02,
                    policy_type: 0,
                    policy_target: Address::ZERO
                }
            );
        });
    }

    /// `DELEGATE || delegate_account(20) || ECRECOVER || nested_sig`.
    fn delegate_auth(delegate_account: Address, nested_key: &K256SigningKey) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(delegate_account.as_slice());
        data.extend_from_slice(Eip8130Constants::ECRECOVER_AUTHENTICATOR.as_slice());
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
                .write(pack(Eip8130Constants::ECRECOVER_AUTHENTICATOR, 0, 0, 0))
                .unwrap();
            // Outer delegate actor on the originating account carries the surface.
            acc.actor_config
                .at_mut(&outer_id)
                .at_mut(&ACCOUNT)
                .write(pack(Eip8130Contracts::DELEGATE_AUTHENTICATOR, 0x08, 0, 0))
                .unwrap();
            let resolved =
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW).unwrap();
            assert_eq!(
                resolved,
                ResolvedActor {
                    actor_id: outer_id,
                    scope: 0x08,
                    policy_type: 0,
                    policy_target: Address::ZERO
                }
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
            // Nested actor is bound on B but scoped PAYER only (no SIGNATURE bit),
            // so `verifySignature(delegate, ...)` would reject it on-chain.
            acc.actor_config
                .at_mut(&nested_id)
                .at_mut(&delegate_account)
                .write(pack(
                    Eip8130Constants::ECRECOVER_AUTHENTICATOR,
                    Eip8130Constants::SCOPE_PAYER,
                    0,
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
            // Nested actor scoped exactly SIGNATURE satisfies the delegate gate.
            acc.actor_config
                .at_mut(&nested_id)
                .at_mut(&delegate_account)
                .write(pack(
                    Eip8130Constants::ECRECOVER_AUTHENTICATOR,
                    Eip8130Constants::SCOPE_SIGNATURE,
                    0,
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
                .write(pack(Eip8130Contracts::DELEGATE_AUTHENTICATOR, 0x08, 0, 0))
                .unwrap();
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::NotBound {
                    actor_id: nested_id,
                    authenticator: Eip8130Constants::ECRECOVER_AUTHENTICATOR,
                }),
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
                .write(pack(Eip8130Constants::ECRECOVER_AUTHENTICATOR, 0, 0, 0))
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
                .write(pack(Eip8130Constants::ECRECOVER_AUTHENTICATOR, 0, 0, 0))
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
    fn revoked_authenticator_is_rejected() {
        let auth = blob(Eip8130Constants::REVOKED_AUTHENTICATOR, &[0u8; 65]);
        with_storage(|acc| {
            assert_eq!(
                ActorAuthorizer::authenticate_actor(acc, ACCOUNT, HASH, &auth, NOW),
                Err(AuthorizeError::Authenticate(AuthError::Revoked)),
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
