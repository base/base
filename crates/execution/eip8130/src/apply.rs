//! The EIP-8130 account-changes apply step: the state mutations the
//! [`ConfigChangeAuthorizer`] deliberately defers, plus account creation and
//! delegation, mirroring `AccountConfiguration`'s write semantics.
//!
//! [`ConfigChangeAuthorizer`] authenticates a config change and gates it on
//! admin scope (`scope == 0`), but does not decode each [`ActorChange`]'s `data` or mutate
//! `actor_config`; that is this module's job. It is the native mirror of
//! `AccountConfiguration.applySignedActorChanges`'s mutation tail
//! (`_authorizeActor` / `_revokeActor` / `_slicePolicy`), of `createAccount` /
//! `_initializeAccount`, and of the deterministic CREATE2 address derivation.
//!
//! Two effects of an account change touch the *account's code* rather than the
//! `AccountConfiguration` storage this crate owns — deploying a created
//! account's bytecode and writing an [EIP-7702]-style delegation indicator. The
//! applier performs every `AccountConfiguration` storage transition itself and
//! surfaces those code writes as an [`AppliedAccountChanges`] for the execution
//! layer (which holds the account/state-trie handle) to carry out.
//!
//! [`ConfigChangeAuthorizer`]: crate::ConfigChangeAuthorizer
//! [`ActorChange`]: base_common_consensus::ActorChange
//! [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::{SolValue, sol};
use base_common_consensus::{
    ActorChange, ActorChangeType, CreateEntry, Eip8130Constants, Eip8130Contracts, InitialActor,
};
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::state::Bytecode;

use crate::{AccountConfigurationStorage, ActorConfig};

sol! {
    /// ABI shape of the per-actor config carried in an `Authorize` change's
    /// `data` (`abi.encode(ActorConfig, bytes policyData)`), matching
    /// `AccountConfiguration.ActorConfig`. Field order and widths are positional
    /// for ABI decoding.
    struct ActorConfigAbi {
        address authenticator;
        uint8 scope;
        uint48 expiry;
    }
}

/// Reason an account change could not be applied.
///
/// Every variant is a hard rejection while applying EIP-8130 state changes: a
/// transaction MUST NOT be included if applying its account changes fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    /// An EIP-8130 state read or write failed.
    #[error("EIP-8130 state access failed: {0}")]
    Storage(#[from] BasePrecompileError),

    /// An `Authorize` change's `data` did not ABI-decode to
    /// `(ActorConfig, bytes policyData)`. Mirrors the `abi.decode` revert.
    #[error("malformed actor-change authorize data")]
    MalformedAuthorizeData,

    /// The new actor's authenticator is `address(0)`, below the valid
    /// authenticator namespace. Mirrors `require(config.authenticator >= K1)`.
    #[error("authenticator address(0) is not a valid selector")]
    InvalidAuthenticator,

    /// `policyData` did not match the actor's `SCOPE_POLICY` bit (non-empty for
    /// an ungated actor, or not exactly `manager(20) || commitment(32)` for a
    /// gated actor). Mirrors `_slicePolicy`.
    #[error("policy data does not match policy type")]
    MalformedPolicyData,

    /// Revoking an actor that is not currently authorized. Mirrors
    /// `_revokeActor`'s `require(isActor(...))`.
    #[error("actor {actor_id} is not authorized and cannot be revoked")]
    NotAnActor {
        /// The actor id that was not an authorized actor.
        actor_id: B256,
    },

    /// A create entry had no initial actors. Mirrors
    /// `require(initialActors.length > 0)`.
    #[error("create entry has no initial actors")]
    NoInitialActors,

    /// A create entry's initial actors are not strictly ascending by actor id
    /// (rejects duplicates and unsorted input). Mirrors
    /// `require(initialActors[i].actorId > previousActorId)`.
    #[error("create initial actors must be strictly ascending by actor id")]
    UnsortedInitialActors,

    /// A create entry's bytecode exceeds the 0xFFFF deployment limit. Mirrors
    /// `require(n <= 0xFFFF)`.
    #[error("create bytecode exceeds the 65535-byte limit")]
    BytecodeTooLarge,

    /// The account targeted by a create entry already has EIP-8130 state. Mirrors
    /// the CREATE2 collision that makes `createAccount` unrepeatable.
    #[error("account {account} is already created")]
    AlreadyCreated {
        /// The counterfactual address that already holds state.
        account: Address,
    },

    /// A create entry's derived address does not equal the transaction sender it
    /// must create. The sender of a create transaction is bound to the create
    /// entry's deterministic deploy address.
    #[error("create address {derived} does not match the bound sender {sender}")]
    CreateAddressMismatch {
        /// The CREATE2 address derived from the create entry.
        derived: Address,
        /// The transaction sender the create entry was expected to produce.
        sender: Address,
    },

    /// More than one create entry, or a create entry not at index 0. Per the
    /// spec a transaction creates at most one account, in the first entry.
    #[error("at most one create entry is allowed, at index 0")]
    InvalidCreatePosition,

    /// More than one delegation entry in a single transaction.
    #[error("at most one delegation entry is allowed")]
    MultipleDelegations,

    /// A delegation entry appears in the same transaction as a create entry.
    /// These are mutually exclusive: a create establishes the account's initial
    /// state (code is set by the protocol) and a delegation modifies an
    /// existing account's code. Having both is undefined by the spec and
    /// rejected as a structural invariant violation.
    #[error("a create entry and a delegation entry may not coexist in the same transaction")]
    CreateAndDelegation,

    /// A delegation attempted to replace ordinary contract bytecode. Delegation
    /// may replace only empty code or code beginning with the delegation
    /// indicator prefix.
    #[error("delegation cannot replace non-delegation code at account {account}")]
    NonDelegatableCode {
        /// The account whose existing code cannot be replaced by a delegation.
        account: Address,
    },

    /// A channel sequence would overflow `u64`.
    #[error("account-change sequence overflow")]
    SequenceOverflow,
}

/// A created account's deferred code write: its counterfactual address and the
/// runtime bytecode the execution layer must install there.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CreatedAccount {
    /// The CREATE2 address the account is deployed at.
    pub address: Address,
    /// The runtime bytecode to install at [`Self::address`].
    pub code: Bytes,
}

/// A delegation's deferred code write against an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DelegationEffect {
    /// The account whose code the delegation indicator is written to (cleared).
    pub account: Address,
    /// The delegation target; `address(0)` clears the existing delegation.
    pub target: Address,
}

impl DelegationEffect {
    /// Creates a deferred delegation code effect.
    #[must_use]
    pub const fn new(account: Address, target: Address) -> Self {
        Self { account, target }
    }

    /// Returns whether `code` may be replaced by a delegation entry.
    ///
    /// Empty code and any code beginning with the delegation indicator prefix
    /// are replaceable. The prefix match is intentional: this does not require
    /// the code to have the canonical 23-byte indicator length.
    #[must_use]
    pub fn can_replace_code(code: &[u8]) -> bool {
        code.is_empty() || code.starts_with(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX)
    }

    /// Installs or clears this delegation after verifying the account's current
    /// code is delegatable.
    ///
    /// The current full bytecode is read before any code write. Ordinary
    /// contract bytecode is left unchanged and rejected with
    /// [`ApplyError::NonDelegatableCode`].
    pub fn install(&self, sctx: StorageCtx<'_>) -> Result<(), ApplyError> {
        let can_replace = sctx.with_account_code(self.account, |code| {
            Ok(Self::can_replace_code(code.original_bytes().as_ref()))
        })?;
        if !can_replace {
            return Err(ApplyError::NonDelegatableCode { account: self.account });
        }

        let code = if self.target.is_zero() {
            Bytecode::default()
        } else {
            Bytecode::new_eip7702(self.target)
        };
        sctx.set_code(self.account, code)?;
        Ok(())
    }

    /// The delegation-indicator code to install
    /// (`DELEGATION_INDICATOR_PREFIX || target`), or `None` to clear the
    /// account's delegation (a zero target).
    #[must_use]
    pub fn indicator(&self) -> Option<Bytes> {
        if self.target.is_zero() {
            return None;
        }
        let mut code = Vec::with_capacity(Eip8130Constants::DELEGATION_INDICATOR_SIZE);
        code.extend_from_slice(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX);
        code.extend_from_slice(self.target.as_slice());
        Some(Bytes::from(code))
    }
}

/// The deferred account-*code* effects produced by applying a transaction's
/// account changes. All `AccountConfiguration` *storage* transitions are already
/// applied; these are the writes the execution layer must perform against the
/// account/state trie.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AppliedAccountChanges {
    /// The account created by a create entry, if any.
    pub created: Option<CreatedAccount>,
    /// The delegation set or cleared by a delegation entry, if any.
    pub delegation: Option<DelegationEffect>,
}

/// Applies EIP-8130 account changes to an [`AccountConfigurationStorage`] view,
/// mirroring `AccountConfiguration`'s write semantics.
///
/// Authentication and scope gating are the [`ConfigChangeAuthorizer`]'s job and
/// must have run first; this step performs the structural-invariant `require`s
/// (`_authorizeActor` / `_revokeActor`) and the state mutations.
///
/// [`ConfigChangeAuthorizer`]: crate::ConfigChangeAuthorizer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountChangeApplier;

impl AccountChangeApplier {
    /// Applies one authorized config change's actor changes against `account`,
    /// advancing the change-sequence channel selected by `chain_id` (`0` =
    /// multichain, else local). Mirrors the mutation tail of
    /// `applySignedActorChanges`.
    pub fn apply_config_change(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        actor_changes: &[ActorChange],
        chain_id: u64,
    ) -> Result<(), ApplyError> {
        // Advance the channel sequence (post-increment in the contract; the
        // authenticated digest committed to the pre-increment value).
        let mut state = storage.get_account_state(account)?;
        if chain_id == 0 {
            state.multichain_sequence =
                state.multichain_sequence.checked_add(1).ok_or(ApplyError::SequenceOverflow)?;
        } else {
            state.local_sequence =
                state.local_sequence.checked_add(1).ok_or(ApplyError::SequenceOverflow)?;
        }
        storage.set_account_state(account, state)?;

        for change in actor_changes {
            match change.change_type {
                ActorChangeType::Authorize => {
                    let (config, policy_data) = Self::decode_authorize(&change.data)?;
                    Self::authorize_actor(storage, account, change.actor_id, config, &policy_data)?;
                }
                ActorChangeType::Revoke => {
                    Self::revoke_actor(storage, account, change.actor_id)?;
                }
            }
        }
        Ok(())
    }

    /// Authorizes (writes) one actor against `account`. Mirrors `_authorizeActor`,
    /// which is an **upsert**: authorizing an already-configured `actor_id`
    /// overwrites its config in place (the end state equals revoke-then-authorize;
    /// observers see another `ActorAuthorized`, last-write-wins). Handles the
    /// mutually-exclusive secp256k1-self vs non-k1-self homes and resets the
    /// policy slots so stale policy can't leak.
    pub fn authorize_actor(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        config: ActorConfig,
        policy_data: &[u8],
    ) -> Result<(), ApplyError> {
        // Authenticator namespace: address(0) is the empty-slot sentinel, never a
        // valid selector (`require(config.authenticator >= K1_AUTHENTICATOR)`).
        if config.authenticator.is_zero() {
            return Err(ApplyError::InvalidAuthenticator);
        }

        let (manager, commitment) = Self::slice_policy(config.scope, policy_data)?;
        let self_id = AccountConfigurationStorage::self_actor_id(account);

        if actor_id == self_id {
            let mut state = storage.get_account_state(account)?;
            if config.authenticator == Eip8130Constants::K1_AUTHENTICATOR {
                // Upsert: overwrite a live self in place (no re-authorize guard);
                // the end state equals revoke-then-authorize.
                // Mutual exclusion: drop any non-k1 self and move into the inline home.
                storage.clear_actor_config(account, actor_id)?;
                state.default_eoa_scope = config.scope;
                state.default_eoa_expiry = config.expiry;
                state.flags &= !Eip8130Constants::DEFAULT_EOA_REVOKED;
            } else {
                // Upsert: overwrite any existing non-k1 self in place.
                storage.set_actor_config(account, actor_id, config)?;
                // Mutual exclusion: disable and clear the inline k1 self.
                state.flags |= Eip8130Constants::DEFAULT_EOA_REVOKED;
                state.default_eoa_scope = 0;
                state.default_eoa_expiry = 0;
            }
            storage.set_account_state(account, state)?;
            // Policy manager/commitment share the actor-id keyspace across both
            // self homes: writing both (zero clears) resets then sets in one step.
            storage.set_policy(account, actor_id, manager, commitment)?;
            return Ok(());
        }

        // Non-self actor: a single `actor_config` home. Upsert: overwrite in
        // place. `set_policy` writes both policy slots (zero clears), resetting
        // any stale policy so an actor moving policy-bearing -> none can't leak
        // state, preserving the "commitment non-zero iff SCOPE_POLICY is set"
        // invariant.
        storage.set_actor_config(account, actor_id, config)?;
        storage.set_policy(account, actor_id, manager, commitment)?;
        Ok(())
    }

    /// Revokes (clears) one actor on `account`. Mirrors `_revokeActor`: clears the
    /// `actor_config` and policy slots, and for the self-actor disables the inline
    /// secp256k1 key by setting `DEFAULT_EOA_REVOKED`.
    pub fn revoke_actor(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
    ) -> Result<(), ApplyError> {
        if !storage.is_actor(account, actor_id)? {
            return Err(ApplyError::NotAnActor { actor_id });
        }
        storage.clear_actor_config(account, actor_id)?;
        storage.clear_policy(account, actor_id)?;
        if actor_id == AccountConfigurationStorage::self_actor_id(account) {
            let mut state = storage.get_account_state(account)?;
            state.flags |= Eip8130Constants::DEFAULT_EOA_REVOKED;
            state.default_eoa_scope = 0;
            state.default_eoa_expiry = 0;
            storage.set_account_state(account, state)?;
        }
        Ok(())
    }

    /// Creates the account described by `entry`: derives its CREATE2 address,
    /// initializes its `AccountConfiguration` state and initial actors, and
    /// returns the deferred bytecode deployment. Mirrors `createAccount`.
    pub fn apply_create(
        storage: &mut AccountConfigurationStorage<'_>,
        entry: &CreateEntry,
    ) -> Result<CreatedAccount, ApplyError> {
        let address = Self::compute_address(entry.user_salt, &entry.code, &entry.initial_actors)?;
        // Block re-initialization of an account that already holds EIP-8130 state.
        // `local_sequence` doubles as the created/imported flag; `multichain_sequence`
        // additionally guards an account that established state via a global
        // (chain_id 0) config change without ever being created/imported. This must
        // be explicit now that `authorize_actor` is an upsert and no longer reverts
        // on a duplicate initial actor (mirrors `createAccount`'s guard).
        let mut state = storage.get_account_state(address)?;
        if state.local_sequence != 0 || state.multichain_sequence != 0 {
            return Err(ApplyError::AlreadyCreated { account: address });
        }

        // Mark initialized and disable the implicit default-EOA path by default
        // (a created account has contract code, so the recovered==account path is
        // unreachable). Written before initializing actors so a self-actorId k1
        // initial actor can re-enable the inline self.
        state.local_sequence = 1;
        state.flags = Eip8130Constants::DEFAULT_EOA_REVOKED;
        storage.set_account_state(address, state)?;

        Self::initialize_actors(storage, address, &entry.initial_actors)?;

        Ok(CreatedAccount { address, code: entry.code.clone() })
    }

    /// Registers a create entry's initial actors, enforcing the non-empty and
    /// strictly-ascending invariants. Each actor carries its `scope` and
    /// `policyData` verbatim (validated by `authorizeActor`'s frozen `policyData`
    /// rule); `expiry` is not expressible at create and is always `0`. Mirrors
    /// `_initializeAccount`.
    fn initialize_actors(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        initial_actors: &[InitialActor],
    ) -> Result<(), ApplyError> {
        if initial_actors.is_empty() {
            return Err(ApplyError::NoInitialActors);
        }
        let mut previous = B256::ZERO;
        for actor in initial_actors {
            if actor.actor_id <= previous {
                return Err(ApplyError::UnsortedInitialActors);
            }
            previous = actor.actor_id;
            // Scope is verbatim and expiry is forced to 0 at create; `policyData`
            // is validated and written by `authorize_actor` / `slice_policy`.
            let config =
                ActorConfig { authenticator: actor.authenticator, scope: actor.scope, expiry: 0 };
            Self::authorize_actor(storage, account, actor.actor_id, config, &actor.policy_data)?;
        }
        Ok(())
    }

    /// Decodes an `Authorize` change's `data` into `(ActorConfig, policyData)`.
    fn decode_authorize(data: &[u8]) -> Result<(ActorConfig, Bytes), ApplyError> {
        let (abi, policy_data) = <(ActorConfigAbi, Bytes)>::abi_decode_params(data)
            .map_err(|_| ApplyError::MalformedAuthorizeData)?;
        let config = ActorConfig {
            authenticator: abi.authenticator,
            scope: abi.scope,
            expiry: abi.expiry.to::<u64>(),
        };
        Ok((config, policy_data))
    }

    /// Validates `policy_data` against `scope`, returning `(manager,
    /// commitment)`. Mirrors `_slicePolicy`: an actor without `SCOPE_POLICY`
    /// requires empty data; a gated actor requires exactly
    /// `manager(20) || commitment(32)`, written verbatim. Neither field need be
    /// nonzero — a zero `commitment` is a valid "no parameters" value and a zero
    /// `manager` gates the key to `address(0)` (no productive target).
    pub fn slice_policy(scope: u8, policy_data: &[u8]) -> Result<(Address, B256), ApplyError> {
        if scope & Eip8130Constants::SCOPE_POLICY == 0 {
            if !policy_data.is_empty() {
                return Err(ApplyError::MalformedPolicyData);
            }
            return Ok((Address::ZERO, B256::ZERO));
        }
        if policy_data.len() != Eip8130Constants::POLICY_DATA_LEN {
            return Err(ApplyError::MalformedPolicyData);
        }
        let manager = Address::from_slice(&policy_data[..20]);
        let commitment = B256::from_slice(&policy_data[20..Eip8130Constants::POLICY_DATA_LEN]);
        Ok((manager, commitment))
    }

    /// Computes the counterfactual CREATE2 address for a created account. Mirrors
    /// `computeAddress`.
    pub fn compute_address(
        user_salt: B256,
        code: &[u8],
        initial_actors: &[InitialActor],
    ) -> Result<Address, ApplyError> {
        let effective_salt = Self::effective_salt(user_salt, initial_actors);
        let code_hash = keccak256(Self::build_deployment_code(code)?);
        let mut buf = Vec::with_capacity(1 + 20 + 32 + 32);
        buf.push(0xff);
        buf.extend_from_slice(Eip8130Contracts::ACCOUNT_CONFIG.as_slice());
        buf.extend_from_slice(effective_salt.as_slice());
        buf.extend_from_slice(code_hash.as_slice());
        Ok(Address::from_word(keccak256(buf)))
    }

    /// The CREATE2 salt: `keccak256(user_salt || actors_commitment)`. Mirrors
    /// `_computeEffectiveSalt`.
    fn effective_salt(user_salt: B256, initial_actors: &[InitialActor]) -> B256 {
        // Exactly 64 bytes: `user_salt` (32) || `actors_commitment` hash (32).
        let mut packed = Vec::with_capacity(64);
        packed.extend_from_slice(user_salt.as_slice());
        packed.extend_from_slice(Self::actors_commitment(initial_actors).as_slice());
        keccak256(packed)
    }

    /// The packed commitment over the initial actor set. The per-actor
    /// contribution is `actorId(32) || authenticator(20) || scope(1) ||
    /// policyData` — 53 bytes for a non-policy actor, 105 bytes when `POLICY`
    /// is set (appending `manager (20) || commitment (32)`). `expiry` does not
    /// participate. The per-actor length is fully determined by `scope`, so the
    /// concatenation is unambiguous. Mirrors `_computeActorsCommitment`.
    fn actors_commitment(initial_actors: &[InitialActor]) -> B256 {
        let mut packed = Vec::with_capacity(initial_actors.len() * 53);
        for actor in initial_actors {
            packed.extend_from_slice(actor.actor_id.as_slice());
            packed.extend_from_slice(actor.authenticator.as_slice());
            packed.push(actor.scope);
            packed.extend_from_slice(&actor.policy_data);
        }
        keccak256(packed)
    }

    /// Builds an account's deployment code: a 14-byte EVM loader header that
    /// returns the trailing `bytecode` as the account's runtime code. Mirrors
    /// `_buildDeploymentCode`.
    pub fn build_deployment_code(bytecode: &[u8]) -> Result<Vec<u8>, ApplyError> {
        let n = bytecode.len();
        if n > 0xFFFF {
            return Err(ApplyError::BytecodeTooLarge);
        }
        let hi = (n >> 8) as u8;
        let lo = n as u8;
        let mut code = vec![
            0x61, hi, lo, // PUSH2 n
            0x60, 0x0E, // PUSH1 14 (code offset)
            0x60, 0x00, // PUSH1 0 (mem dest)
            0x39, // CODECOPY
            0x61, hi, lo, // PUSH2 n
            0x60, 0x00, // PUSH1 0 (mem offset)
            0xF3, // RETURN
        ];
        code.extend_from_slice(bytecode);
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use base_precompile_storage::{HashMapStorageProvider, PrecompileStorageProvider, StorageCtx};
    use revm::state::Bytecode;

    use super::*;

    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000a1");
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;
    const AUTHENTICATOR: Address = address!("0x00000000000000000000000000000000000000bb");
    const MANAGER: Address = address!("0x00000000000000000000000000000000000000cc");
    const COMMITMENT: B256 =
        b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
    const NON_SELF: B256 =
        b256!("0x00000000000000000000000000000000000000dd000000000000000000000000");

    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    /// `abi.encode(ActorConfig, bytes policyData)` for an authorize change.
    fn authorize_data(config: &ActorConfig, policy_data: &[u8]) -> Bytes {
        let abi = ActorConfigAbi {
            authenticator: config.authenticator,
            scope: config.scope,
            expiry: alloy_primitives::aliases::U48::from(config.expiry),
        };
        Bytes::from((abi, Bytes::copy_from_slice(policy_data)).abi_encode_params())
    }

    fn ungated(authenticator: Address, scope: u8) -> ActorConfig {
        ActorConfig { authenticator, scope, expiry: 0 }
    }

    #[test]
    fn slice_policy_matches_contract() {
        assert_eq!(
            AccountChangeApplier::slice_policy(0, &[]).unwrap(),
            (Address::ZERO, B256::ZERO)
        );
        assert_eq!(
            AccountChangeApplier::slice_policy(0, &[1]),
            Err(ApplyError::MalformedPolicyData)
        );

        let mut data = Vec::new();
        data.extend_from_slice(MANAGER.as_slice());
        data.extend_from_slice(COMMITMENT.as_slice());
        assert_eq!(
            AccountChangeApplier::slice_policy(Eip8130Constants::SCOPE_POLICY, &data).unwrap(),
            (MANAGER, COMMITMENT)
        );

        // Wrong length rejects.
        assert_eq!(
            AccountChangeApplier::slice_policy(Eip8130Constants::SCOPE_POLICY, &data[..51]),
            Err(ApplyError::MalformedPolicyData)
        );
        // Per the frozen rule, neither field need be nonzero: a zero
        // manager/commitment is well-formed (`manager(20) || commitment(32)`).
        let zero_mgr = [0u8; 52];
        assert_eq!(
            AccountChangeApplier::slice_policy(Eip8130Constants::SCOPE_POLICY, &zero_mgr).unwrap(),
            (Address::ZERO, B256::ZERO)
        );
    }

    #[test]
    fn authorize_and_revoke_non_self_actor() {
        with_storage(|acc| {
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SENDER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, config, &[]).unwrap();
            assert_eq!(acc.get_actor_config(ACCOUNT, NON_SELF).unwrap(), config);
            assert!(acc.is_actor(ACCOUNT, NON_SELF).unwrap());

            // Upsert: re-authorizing an occupied slot overwrites it in place.
            let rescoped = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SELF_PAYER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, rescoped, &[]).unwrap();
            assert_eq!(acc.get_actor_config(ACCOUNT, NON_SELF).unwrap(), rescoped);

            // Revoke clears the slot.
            AccountChangeApplier::revoke_actor(acc, ACCOUNT, NON_SELF).unwrap();
            assert!(acc.get_actor_config(ACCOUNT, NON_SELF).unwrap().is_empty());
            assert_eq!(
                AccountChangeApplier::revoke_actor(acc, ACCOUNT, NON_SELF),
                Err(ApplyError::NotAnActor { actor_id: NON_SELF })
            );
        });
    }

    #[test]
    fn authorize_zero_authenticator_rejected() {
        with_storage(|acc| {
            let config = ungated(Address::ZERO, 0);
            assert_eq!(
                AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, config, &[]),
                Err(ApplyError::InvalidAuthenticator)
            );
        });
    }

    #[test]
    fn policy_scope_controls_policy_data() {
        with_storage(|acc| {
            let mut data = Vec::new();
            data.extend_from_slice(MANAGER.as_slice());
            data.extend_from_slice(COMMITMENT.as_slice());

            // Policy data without SCOPE_POLICY is rejected.
            let unrestricted = ActorConfig { authenticator: AUTHENTICATOR, scope: 0, expiry: 0 };
            assert_eq!(
                AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, unrestricted, &data),
                Err(ApplyError::MalformedPolicyData)
            );

            // SCOPE_POLICY actor accepted; policy slots written.
            let ok = ActorConfig {
                authenticator: AUTHENTICATOR,
                scope: Eip8130Constants::SCOPE_POLICY,
                expiry: 0,
            };
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, ok, &data).unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, NON_SELF).unwrap(), (MANAGER, COMMITMENT));
        });
    }

    #[test]
    fn reauthorize_to_policy_none_clears_policy_slots() {
        with_storage(|acc| {
            let mut data = Vec::new();
            data.extend_from_slice(MANAGER.as_slice());
            data.extend_from_slice(COMMITMENT.as_slice());

            // Authorize a policy-bearing actor; policy slots populated.
            let gated = ActorConfig {
                authenticator: AUTHENTICATOR,
                scope: Eip8130Constants::SCOPE_POLICY,
                expiry: 0,
            };
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, gated, &data).unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, NON_SELF).unwrap(), (MANAGER, COMMITMENT));

            // Upsert the same actor down to no policy: the stale manager/commitment
            // must be cleared (policy slots are written only while SCOPE_POLICY is set).
            let ungated_cfg = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SENDER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, ungated_cfg, &[])
                .unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, NON_SELF).unwrap(), (Address::ZERO, B256::ZERO));
            assert_eq!(acc.get_policy_manager(ACCOUNT, NON_SELF).unwrap(), Address::ZERO);
        });
    }

    #[test]
    fn authorize_self_k1_enables_inline_and_revoke_disables() {
        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
            // Account starts with the inline self live (flag unset, all-zero inline).
            let scoped = ungated(K1, Eip8130Constants::SCOPE_SENDER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, scoped, &[]).unwrap();
            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert!(!state.default_eoa_revoked());
            assert_eq!(state.default_eoa_scope, Eip8130Constants::SCOPE_SENDER);
            // No explicit actor_config slot is used for the k1 self.
            assert!(acc.get_actor_config(ACCOUNT, self_id).unwrap().is_empty());

            // Upsert: re-authorizing a live self rescopes the inline config in
            // place (no prior revoke required).
            let rescoped = ungated(K1, Eip8130Constants::SCOPE_SELF_PAYER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, rescoped, &[]).unwrap();
            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert!(!state.default_eoa_revoked());
            assert_eq!(state.default_eoa_scope, Eip8130Constants::SCOPE_SELF_PAYER);

            // Revoke sets the flag and clears the inline config.
            AccountChangeApplier::revoke_actor(acc, ACCOUNT, self_id).unwrap();
            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert!(state.default_eoa_revoked());
            assert_eq!(state.default_eoa_scope, 0);
        });
    }

    #[test]
    fn authorize_self_non_k1_disables_inline_eoa() {
        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
            let config = ungated(AUTHENTICATOR, 0);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, config, &[]).unwrap();
            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert!(state.default_eoa_revoked());
            assert_eq!(acc.get_actor_config(ACCOUNT, self_id).unwrap(), config);
        });
    }

    #[test]
    fn config_change_advances_sequence_and_applies() {
        with_storage(|acc| {
            // Authorize then revoke a non-self actor in one multichain change.
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SENDER);
            let changes = vec![ActorChange {
                change_type: ActorChangeType::Authorize,
                actor_id: NON_SELF,
                data: authorize_data(&config, &[]),
            }];
            AccountChangeApplier::apply_config_change(acc, ACCOUNT, &changes, 0).unwrap();
            assert_eq!(acc.get_change_sequences(ACCOUNT).unwrap(), (1, 0));
            assert!(acc.is_actor(ACCOUNT, NON_SELF).unwrap());

            // A local-channel change advances the local sequence instead.
            AccountChangeApplier::apply_config_change(acc, ACCOUNT, &[], 8453).unwrap();
            assert_eq!(acc.get_change_sequences(ACCOUNT).unwrap(), (1, 1));
        });
    }

    #[test]
    fn build_deployment_code_matches_contract_layout() {
        let bytecode = [0xAA, 0xBB, 0xCC];
        let code = AccountChangeApplier::build_deployment_code(&bytecode).unwrap();
        let n = bytecode.len() as u8;
        assert_eq!(
            &code[..14],
            &[0x61, 0x00, n, 0x60, 0x0E, 0x60, 0x00, 0x39, 0x61, 0x00, n, 0x60, 0x00, 0xF3]
        );
        assert_eq!(&code[14..], &bytecode);
        assert!(AccountChangeApplier::build_deployment_code(&vec![0u8; 0x10000]).is_err());
    }

    #[test]
    fn create_initializes_state_actors_and_address() {
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: b256!(
                    "0x2222222222222222222222222222222222222222222222222222222222222222"
                ),
                code: Bytes::from_static(&[0x60, 0x00]),
                initial_actors: vec![InitialActor::owner(NON_SELF, AUTHENTICATOR)],
            };
            let expected = AccountChangeApplier::compute_address(
                entry.user_salt,
                &entry.code,
                &entry.initial_actors,
            )
            .unwrap();

            let created = AccountChangeApplier::apply_create(acc, &entry).unwrap();
            assert_eq!(created.address, expected);
            assert_eq!(created.code, entry.code);

            // State: initialized (local_sequence == 1) with the default EOA revoked.
            let state = acc.get_account_state(expected).unwrap();
            assert_eq!(state.local_sequence, 1);
            assert!(state.default_eoa_revoked());
            // Initial actor registered as an unrestricted owner.
            assert_eq!(
                acc.get_actor_config(expected, NON_SELF).unwrap(),
                ungated(AUTHENTICATOR, 0)
            );

            // Re-creating the same account is rejected.
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &entry),
                Err(ApplyError::AlreadyCreated { account: expected })
            );
        });
    }

    #[test]
    fn create_rejected_when_account_has_only_multichain_state() {
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: b256!(
                    "0x3333333333333333333333333333333333333333333333333333333333333333"
                ),
                code: Bytes::from_static(&[0x60, 0x00]),
                initial_actors: vec![InitialActor::owner(NON_SELF, AUTHENTICATOR)],
            };
            let expected = AccountChangeApplier::compute_address(
                entry.user_salt,
                &entry.code,
                &entry.initial_actors,
            )
            .unwrap();

            // Account established global (chain_id 0) state without ever being
            // created/imported: local_sequence == 0 but multichain_sequence != 0.
            let mut state = acc.get_account_state(expected).unwrap();
            state.multichain_sequence = 1;
            acc.set_account_state(expected, state).unwrap();

            // create must still reject (the guard checks both sequences).
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &entry),
                Err(ApplyError::AlreadyCreated { account: expected })
            );
        });
    }

    #[test]
    fn create_requires_sorted_non_empty_actors() {
        with_storage(|acc| {
            let empty =
                CreateEntry { user_salt: B256::ZERO, code: Bytes::new(), initial_actors: vec![] };
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &empty),
                Err(ApplyError::NoInitialActors)
            );

            let unsorted = CreateEntry {
                user_salt: B256::ZERO,
                code: Bytes::new(),
                initial_actors: vec![
                    InitialActor::owner(B256::repeat_byte(2), AUTHENTICATOR),
                    InitialActor::owner(B256::repeat_byte(1), AUTHENTICATOR),
                ],
            };
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &unsorted),
                Err(ApplyError::UnsortedInitialActors)
            );
        });
    }

    #[test]
    fn delegation_effect_indicator_set_and_clear() {
        let target = address!("0x00000000000000000000000000000000000000ee");
        let set = DelegationEffect::new(ACCOUNT, target);
        let code = set.indicator().unwrap();
        assert_eq!(code.len(), Eip8130Constants::DELEGATION_INDICATOR_SIZE);
        assert_eq!(&code[..3], &Eip8130Constants::DELEGATION_INDICATOR_PREFIX);
        assert_eq!(&code[3..], target.as_slice());

        let clear = DelegationEffect::new(ACCOUNT, Address::ZERO);
        assert!(clear.indicator().is_none());
    }

    #[test]
    fn delegation_effect_replaceable_code_predicate() {
        assert!(DelegationEffect::can_replace_code(&[]));
        assert!(DelegationEffect::can_replace_code(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX));

        let mut full_indicator = Eip8130Constants::DELEGATION_INDICATOR_PREFIX.to_vec();
        full_indicator.extend_from_slice(Address::repeat_byte(0x11).as_slice());
        assert!(DelegationEffect::can_replace_code(&full_indicator));

        assert!(!DelegationEffect::can_replace_code(&[0x60, 0x00]));
        assert!(!DelegationEffect::can_replace_code(&[0xef, 0x01, 0x01]));
    }

    #[test]
    fn delegation_effect_install_rejects_ordinary_code_without_mutating_it() {
        let ordinary = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, ordinary.clone()).unwrap();

        let effect = DelegationEffect::new(ACCOUNT, Address::repeat_byte(0x22));
        let error = StorageCtx::enter(&mut storage, |sctx| effect.install(sctx)).unwrap_err();

        assert_eq!(error, ApplyError::NonDelegatableCode { account: ACCOUNT });
        assert_eq!(
            storage.get_account_info(ACCOUNT).and_then(|info| info.code.as_ref()),
            Some(&ordinary)
        );
    }

    #[test]
    fn delegation_effect_install_accepts_empty_code() {
        let target = Address::repeat_byte(0x33);
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, target).install(sctx)
        })
        .unwrap();

        assert_eq!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .and_then(Bytecode::eip7702_address),
            Some(target)
        );
    }

    #[test]
    fn delegation_effect_install_updates_existing_delegation() {
        let target = Address::repeat_byte(0x44);
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, Bytecode::new_eip7702(Address::repeat_byte(0x11))).unwrap();

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, target).install(sctx)
        })
        .unwrap();

        assert_eq!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .and_then(Bytecode::eip7702_address),
            Some(target)
        );
    }

    #[test]
    fn delegation_effect_install_clears_existing_delegation() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, Bytecode::new_eip7702(Address::repeat_byte(0x11))).unwrap();

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, Address::ZERO).install(sctx)
        })
        .unwrap();

        assert!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .is_some_and(Bytecode::is_empty)
        );
    }
}
