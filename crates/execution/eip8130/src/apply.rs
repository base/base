//! The EIP-8130 account-changes apply step: the state mutations the
//! [`ConfigChangeAuthorizer`] deliberately defers, plus account creation and
//! delegation, mirroring `AccountConfiguration`'s write semantics.
//!
//! [`ConfigChangeAuthorizer`] authenticates a signed batch and gates it on
//! admin scope (`scope == 0`), but does not decode each [`SignedChange`]'s
//! `payload` or mutate `actor_config`; that is this module's job. It is the
//! native mirror of `Keystore.applySignedAccountChanges`'s mutation tail
//! (`_applyAuthorize` / `_applyRevoke` / `_slicePolicy`), of `createAccount` /
//! `_initializeAccount`, and of the deterministic CREATE2 address derivation.
//!
//! Two effects of an account change touch the *account's code* rather than the
//! `AccountConfiguration` storage this crate owns — deploying a created
//! account's bytecode and writing an [EIP-7702]-style delegation indicator. The
//! applier performs every `AccountConfiguration` storage transition itself and
//! surfaces those code writes as an [`AppliedAccountChanges`] for the execution
//! layer (which holds the account/state-trie handle) to carry out.
//!
//! Successful authorize / revoke / create mutations also inject the matching
//! `IAccountConfiguration` receipt logs via [`AccountConfigurationEvents`]
//! (the enshrined path has no EVM LOG opcodes of its own).
//!
//! [`ConfigChangeAuthorizer`]: crate::ConfigChangeAuthorizer
//! [`SignedChange`]: base_common_consensus::SignedChange
//! [`AccountConfigurationEvents`]: crate::AccountConfigurationEvents
//! [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::{SolValue, sol};
use base_common_consensus::{
    AccountChangeChannel, ChangeType, CreateEntry, Eip8130Constants, Eip8130Contracts,
    InitialActor, SignedChange,
};
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::state::Bytecode;

use crate::{AccountConfigurationEvents, AccountConfigurationStorage, AccountState, ActorConfig};

sol! {
    /// ABI shape of the per-actor config carried in an `AuthorizeActor` op's
    /// `payload` (`abi.encode(bytes32 actorId, ActorConfig, bytes policyData)`),
    /// matching `Keystore.ActorConfig`. Field order and widths are positional
    /// for ABI decoding.
    struct ActorConfigAbi {
        address authenticator;
        uint48 expiry;
        uint16 scope;
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

    /// An `AuthorizeActor` op's `payload` did not ABI-decode to
    /// `(bytes32 actorId, ActorConfig, bytes policyData)`. Mirrors the
    /// `abi.decode` revert.
    #[error("malformed actor-change authorize data")]
    MalformedAuthorizeData,

    /// A `RevokeActor` op's `payload` did not ABI-decode to `(bytes32 actorId)`.
    #[error("malformed actor-change revoke data")]
    MalformedRevokeData,

    /// An op that requires an empty `payload` (currently `IncrementLocalEpoch`)
    /// carried a non-empty one. Mirrors `Keystore.InvalidChangePayload`.
    #[error("account-change op payload must be empty")]
    InvalidChangePayload,

    /// An `IncrementLocalEpoch` op could not advance because the local epoch is
    /// at its terminal `u32::MAX` value. Mirrors `Keystore.EpochSaturated`.
    #[error("local epoch is saturated and cannot be incremented")]
    EpochSaturated,

    /// A signed batch carried an environment op (`Lock` or `Unlock`) whose apply
    /// handler is not yet enshrined. These ops are wired into the apply path by a
    /// subsequent change; until then a batch carrying one is rejected rather than
    /// silently ignored.
    #[error("unsupported account-change op in the enshrined apply path")]
    UnsupportedChangeType,

    /// A signed batch carried no changes. Mirrors `applySignedAccountChanges`'s
    /// `revert EmptyChangeSet()`: an empty batch would otherwise consume (advance)
    /// a channel's sequence without altering any configuration. Rejected before
    /// the sequence is advanced.
    #[error("signed account-change batch is empty")]
    EmptyChangeSet,

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
    /// `ActorsNotSortedOrDuplicate`.
    #[error("create initial actors must be strictly ascending by actor id")]
    ActorsNotSortedOrDuplicate,

    /// A create entry's bytecode is empty. Mirrors `_buildDeploymentCode`'s
    /// `revert EmptyBytecode()`: a codeless create is rejected because an account
    /// carrying actor config but no runtime code (nor a delegation) would break
    /// the EOA invariant — a key would exist for an address that is not an EOA.
    #[error("create bytecode is empty")]
    EmptyBytecode,

    /// A create entry's bytecode exceeds the 0xFFFF deployment limit. Mirrors
    /// `require(n <= 0xFFFF)`.
    #[error("create bytecode exceeds the 65535-byte limit")]
    BytecodeTooLarge,

    /// A create entry's runtime code cannot be deployed. Mirrors `createAccount`'s
    /// `AccountDeploymentFailed`: on the contract, `CREATE2` returns `address(0)`
    /// when the runtime code is over the EIP-170 [`Eip8130Constants::MAX_CODE_SIZE`]
    /// cap or leads with the EIP-3541 reserved `0xEF` byte. The enshrined apply
    /// path deploys the code directly (`set_code`) rather than via `CREATE2`, so it
    /// must reject those payloads explicitly — a leading-`0xEF` payload would
    /// otherwise panic in `Bytecode::new_raw` instead of failing the transaction.
    #[error("account {account} runtime code is not deployable (EIP-170 size / EIP-3541 prefix)")]
    AccountDeploymentFailed {
        /// The counterfactual address whose runtime code cannot be deployed.
        account: Address,
    },

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

    /// A delegation targeted an empty-code account that is
    /// [`Eip8130Constants::FLAG_CONTRACT_ESTABLISHED`]. Empty code on a
    /// keystore-established account (e.g. after an EIP-6780 same-transaction
    /// `SELFDESTRUCT`) is not proof of a key-backed EOA, so it must not be
    /// (re)delegated as if it were one — doing so would resurrect a CREATE2
    /// address that no private key controls.
    #[error("delegation cannot target empty-code contract-established account {account}")]
    ContractEstablishedCodeless {
        /// The keystore-established account whose empty code cannot be delegated.
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
        let (can_replace, code_is_empty) = sctx.with_account_code(self.account, |code| {
            let bytes = code.original_bytes();
            let slice = bytes.as_ref();
            Ok((Self::can_replace_code(slice), slice.is_empty()))
        })?;
        if !can_replace {
            return Err(ApplyError::NonDelegatableCode { account: self.account });
        }

        // Empty code on a keystore-established account is not proof of a key-backed
        // EOA (e.g. an EIP-6780 same-transaction SELFDESTRUCT leaves EIP-8130 state
        // behind empty code), so it must not be (re)delegated as if it were one.
        // Reading the AccountConfiguration flag mirrors `Keystore.isContractEstablished`.
        if code_is_empty
            && AccountConfigurationStorage::new(sctx).is_contract_established(self.account)?
        {
            return Err(ApplyError::ContractEstablishedCodeless { account: self.account });
        }

        let code = if self.target.is_zero() {
            Bytecode::default()
        } else {
            Bytecode::new_eip7702(self.target)
        };
        sctx.set_code(self.account, code)?;
        // Protocol-injected: the Solidity contract never emits this on the EVM
        // path; EIP-8130 requires the receipt log for successful delegation updates.
        AccountConfigurationEvents::emit_delegation_applied(sctx, self.account, self.target)?;
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
    /// Applies one authorized signed batch's ops against `account`, advancing the
    /// channel's sequence counter. Mirrors the mutation tail of
    /// `applySignedAccountChanges`.
    /// Returns the number of empty zero-to-zero revoke slots discounted (see
    /// [`Self::revoke_actor_with_account_state`]), for intrinsic-gas discounting.
    pub fn apply_config_change(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        changes: &[SignedChange],
        channel: AccountChangeChannel,
        sequence: u64,
    ) -> Result<u32, ApplyError> {
        let mut state = storage.get_account_state(account)?;
        let revoke_discount_slots = Self::apply_config_change_with_account_state(
            storage, account, changes, channel, sequence, &mut state,
        )?;
        storage.set_account_state(account, state)?;
        Ok(revoke_discount_slots)
    }

    /// Applies one signed batch while carrying an already-loaded account state
    /// through sequence advancement and any inline-self actor mutations.
    ///
    /// The caller owns persistence of `state`, allowing the transaction
    /// orchestrator to perform one final packed-state write after all relevant
    /// mutations in this batch.
    ///
    /// Returns the number of empty zero-to-zero revoke slots discounted (see
    /// [`Self::revoke_actor_with_account_state`]), for intrinsic-gas discounting.
    pub fn apply_config_change_with_account_state(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        changes: &[SignedChange],
        channel: AccountChangeChannel,
        sequence: u64,
        state: &mut AccountState,
    ) -> Result<u32, ApplyError> {
        // Reject an empty batch before advancing the sequence. A no-op batch would
        // otherwise consume the channel's sequence (or initialize a fresh account)
        // without changing any configuration. Mirrors `applySignedAccountChanges`'s
        // `revert EmptyChangeSet()`, which fires before the sequence gate; the
        // txpool rejects the same case up front (`cfg.changes.is_empty()`).
        if changes.is_empty() {
            return Err(ApplyError::EmptyChangeSet);
        }

        Self::advance_channel_sequence(channel, sequence, state)?;

        let mut revoke_discount_slots = 0u32;
        for change in changes {
            match change.change_type {
                ChangeType::AuthorizeActor => {
                    let (actor_id, config, policy_data) = Self::decode_authorize(&change.payload)?;
                    Self::authorize_actor_with_account_state(
                        storage,
                        account,
                        actor_id,
                        config,
                        &policy_data,
                        state,
                    )?;
                }
                ChangeType::RevokeActor => {
                    let actor_id = Self::decode_revoke(&change.payload)?;
                    revoke_discount_slots = revoke_discount_slots.saturating_add(
                        Self::revoke_actor_with_account_state(storage, account, actor_id, state)?,
                    );
                }
                ChangeType::IncrementLocalEpoch => {
                    Self::apply_increment_local_epoch(&change.payload, state)?;
                }
                // Lock / Unlock apply handlers are not yet enshrined.
                ChangeType::Lock | ChangeType::Unlock => {
                    return Err(ApplyError::UnsupportedChangeType);
                }
            }
        }
        Ok(revoke_discount_slots)
    }

    /// Advances the channel's replay counter for an applied batch, mirroring the
    /// epoch/sequence advance at the top of `applySignedAccountChanges`.
    ///
    /// - [`AccountChangeChannel::Local`]: a sequenced batch (low half !=
    ///   [`Eip8130Constants::UNSEQUENCED`]) advances `local_sequence` to
    ///   `seq + 1`. An unsequenced (JIT) batch consumes no counter, but a *first*
    ///   unsequenced batch marks a fresh account initialized (`local_sequence =
    ///   1`), invalidating outstanding sequence-0 signatures.
    /// - [`AccountChangeChannel::Multichain`]: advances `multichain_sequence` to
    ///   `sequence + 1`.
    fn advance_channel_sequence(
        channel: AccountChangeChannel,
        sequence: u64,
        state: &mut AccountState,
    ) -> Result<(), ApplyError> {
        match channel {
            AccountChangeChannel::Local => {
                // Defense-in-depth: a Local sequence word is `epoch(hi 32) ||
                // localSeq(lo 32)`, and the authorizer has already validated the
                // epoch high-half against `state.local_epoch` before apply. Assert
                // it here so a future direct caller of the `pub` apply entrypoints
                // cannot advance the sequence against a stale epoch — the low-half
                // advance below intentionally ignores the epoch bits.
                debug_assert_eq!(
                    (sequence >> 32) as u32,
                    state.local_epoch as u32,
                    "local sequence word epoch must match the account's local epoch",
                );
                let seq = sequence as u32;
                if seq != Eip8130Constants::UNSEQUENCED {
                    state.local_sequence =
                        u64::from(seq).checked_add(1).ok_or(ApplyError::SequenceOverflow)?;
                } else if !state.is_initialized() {
                    state.local_sequence = 1;
                }
            }
            AccountChangeChannel::Multichain => {
                // Symmetric to the Local branch: the authorizer already validated
                // `sequence` against the account's current multichain sequence
                // before apply. Assert it here so a future direct caller of the
                // `pub` apply entrypoints cannot advance against a mismatched
                // sequence.
                debug_assert_eq!(
                    sequence, state.multichain_sequence,
                    "multichain sequence must match the account's current multichain sequence",
                );
                state.multichain_sequence =
                    state.multichain_sequence.checked_add(1).ok_or(ApplyError::SequenceOverflow)?;
            }
        }
        Ok(())
    }

    /// Applies an `IncrementLocalEpoch` op, mirroring `_applyIncrementLocalEpoch`:
    /// a strict `local_epoch += 1` (rejecting the terminal `u32::MAX`) that also
    /// resets `local_sequence` to 0, retiring every unlanded local signature (each
    /// commits the full 64-bit `localEpoch ‖ localSequence` word). Allowed on
    /// either channel — a Multichain batch may bump the local epoch without a
    /// separate Local batch. The payload MUST be empty.
    fn apply_increment_local_epoch(
        payload: &[u8],
        state: &mut AccountState,
    ) -> Result<(), ApplyError> {
        if !payload.is_empty() {
            return Err(ApplyError::InvalidChangePayload);
        }
        // `local_epoch` stores a `uint32`; only the terminal value cannot advance
        // (the epoch half has no reserved sentinel).
        if state.local_epoch == u64::from(u32::MAX) {
            return Err(ApplyError::EpochSaturated);
        }
        state.local_epoch += 1;
        state.local_sequence = 0;
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
        if actor_id == AccountConfigurationStorage::self_actor_id(account) {
            let mut state = storage.get_account_state(account)?;
            Self::authorize_actor_with_account_state(
                storage,
                account,
                actor_id,
                config,
                policy_data,
                &mut state,
            )?;
            storage.set_account_state(account, state)?;
            return Ok(());
        }
        Self::authorize_non_self_actor(storage, account, actor_id, config, policy_data)
    }

    /// Authorizes an actor while applying inline-self changes to `state` without
    /// independently reading or writing the packed account-state slot.
    ///
    /// `state` is only read or mutated when `actor_id` is the account's own
    /// self-actor; a non-self `actor_id` is dispatched to
    /// [`Self::authorize_non_self_actor`], which leaves `state` untouched (the
    /// mixed-actor `apply_config_change_with_account_state` loop relies on this).
    /// Callers must therefore not assume `state` reflects a non-self change.
    pub fn authorize_actor_with_account_state(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        config: ActorConfig,
        policy_data: &[u8],
        state: &mut AccountState,
    ) -> Result<(), ApplyError> {
        // Authenticator namespace: address(0) is the empty-slot sentinel, never a
        // valid selector (`require(config.authenticator >= K1_AUTHENTICATOR)`).
        if config.authenticator.is_zero() {
            return Err(ApplyError::InvalidAuthenticator);
        }

        let self_id = AccountConfigurationStorage::self_actor_id(account);
        if actor_id != self_id {
            return Self::authorize_non_self_actor(storage, account, actor_id, config, policy_data);
        }

        let (manager, commitment) = Self::slice_policy(config.scope, policy_data)?;
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
        // Always touch both policy slots, including zero-to-zero clears. Besides
        // scrubbing stale state, this preserves the reference operation's access
        // warming for the subsequent call phases.
        storage.set_policy(account, actor_id, manager, commitment)?;
        AccountConfigurationEvents::emit_actor_authorized(
            storage, account, actor_id, &config, manager, commitment,
        )?;
        Ok(())
    }

    /// Authorizes an actor whose id is not the account's self id.
    pub fn authorize_non_self_actor(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        config: ActorConfig,
        policy_data: &[u8],
    ) -> Result<(), ApplyError> {
        if config.authenticator.is_zero() {
            return Err(ApplyError::InvalidAuthenticator);
        }
        let (manager, commitment) = Self::slice_policy(config.scope, policy_data)?;
        // Non-self actor: a single `actor_config` home. Upsert: overwrite in
        // place. Both policy slots are always touched so zero-to-zero clears
        // preserve the reference operation's access warming.
        storage.set_actor_config(account, actor_id, config)?;
        storage.set_policy(account, actor_id, manager, commitment)?;
        AccountConfigurationEvents::emit_actor_authorized(
            storage, account, actor_id, &config, manager, commitment,
        )?;
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
        if actor_id == AccountConfigurationStorage::self_actor_id(account) {
            let mut state = storage.get_account_state(account)?;
            Self::revoke_actor_with_account_state(storage, account, actor_id, &mut state)?;
            storage.set_account_state(account, state)?;
            return Ok(());
        }
        let config = storage.actor_config_slot(account, actor_id)?;
        Self::revoke_explicit_actor(storage, account, actor_id, config)
    }

    /// Revokes an actor while applying inline-self changes to `state` without
    /// independently reading or writing the packed account-state slot.
    ///
    /// Returns the number of the revoke's three conservatively reset-priced slots
    /// that were actually empty zero-to-zero touches, for intrinsic-gas discounting
    /// (see [`crate::IntrinsicGasInput::revoke_discount_slots`]):
    ///
    /// - An **explicit** actor revoke returns `0`: its `actor_config` slot is a real
    ///   reset and its policy slots are kept at the conservative reset price.
    /// - An **inline** secp256k1 self revoke returns the count of its three
    ///   conservatively-priced slots that are actually zero: `actor_config` is
    ///   always empty, and each policy slot (`manager`, `commitment`) is counted
    ///   only when its stored value is zero. An ungated self returns `3` (both
    ///   policy slots unwritten); a gated self returns `1`, `2`, or `3` depending
    ///   on how many of its policy slots were written non-zero — a gated actor may
    ///   still carry a zero manager and/or commitment (see [`Self::slice_policy`]),
    ///   which are zero-to-zero no-ops rather than real resets.
    ///
    /// The authorize/create paths maintain mutual exclusion between the inline
    /// secp256k1 self key and an explicit non-k1 self actor. This method does not
    /// rely solely on that coupling: when it revokes an explicit self actor, it
    /// defensively disables and clears the inline key as well, preserving the
    /// invariant even if a future caller installs the explicit entry directly.
    pub fn revoke_actor_with_account_state(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        state: &mut AccountState,
    ) -> Result<u32, ApplyError> {
        let config = storage.actor_config_slot(account, actor_id)?;
        let is_self = actor_id == AccountConfigurationStorage::self_actor_id(account);
        if config.authenticator != Address::ZERO {
            Self::revoke_explicit_actor(storage, account, actor_id, config)?;
            // Defense-in-depth: an explicit self actor is necessarily non-k1, and
            // `authorize_actor_with_account_state`/`apply_create` already set
            // DEFAULT_EOA_REVOKED and zeroed the inline scope/expiry when it was
            // installed. Re-assert that invariant here (a no-op on the in-tree
            // install paths) instead of depending on the authorize path, so a
            // future direct `set_actor_config` writer cannot leave a live inline
            // secp256k1 home behind an explicit self entry. `state` is already
            // loaded and flushed by the caller, so this adds no storage access.
            // This clears a populated explicit `actor_config`, so it is not the
            // discounted inline-self shape.
            if is_self {
                Self::disable_inline_self(state);
            }
            return Ok(0);
        }
        if !is_self || state.default_eoa_revoked() {
            return Err(ApplyError::NotAnActor { actor_id });
        }

        // The inline self's `actor_config` slot is always empty (a zero-to-zero
        // reset no-op). Its two policy slots are written verbatim at authorize
        // time and may be zero even when the self is policy-gated (`slice_policy`
        // permits a zero manager and/or commitment), so a zero slot is likewise a
        // no-op. Count each actually-empty slot from the raw storage before
        // `clear_policy` zeroes them; only the non-zero slots stay at the
        // conservative reset price. The gate bit is not a reliable proxy here.
        let mut empty_slots = 1; // `actor_config`
        if storage.get_policy_manager(account, actor_id)?.is_zero() {
            empty_slots += 1;
        }
        if storage.get_policy_commitment(account, actor_id)?.is_zero() {
            empty_slots += 1;
        }
        storage.clear_actor_config(account, actor_id)?;
        storage.clear_policy(account, actor_id)?;
        Self::disable_inline_self(state);
        AccountConfigurationEvents::emit_actor_revoked(storage, account, actor_id)?;
        Ok(empty_slots)
    }

    /// Disables the inline secp256k1 self key in the packed account-state slot:
    /// sets `DEFAULT_EOA_REVOKED` and zeroes the inline scope/expiry.
    const fn disable_inline_self(state: &mut AccountState) {
        state.flags |= Eip8130Constants::DEFAULT_EOA_REVOKED;
        state.default_eoa_scope = 0;
        state.default_eoa_expiry = 0;
    }

    /// Revokes an actor represented by an explicit `actor_config` entry.
    pub fn revoke_explicit_actor(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        config: ActorConfig,
    ) -> Result<(), ApplyError> {
        if config.authenticator == Address::ZERO {
            return Err(ApplyError::NotAnActor { actor_id });
        }
        storage.clear_actor_config(account, actor_id)?;
        storage.clear_policy(account, actor_id)?;
        AccountConfigurationEvents::emit_actor_revoked(storage, account, actor_id)?;
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
        // `local_sequence` doubles as the created/imported flag; `local_epoch`
        // covers an account whose sequence was reset to 0 by `IncrementLocalEpoch`;
        // and `multichain_sequence` guards an account that established state via a
        // global (chain_id 0) config change without ever being created/imported.
        // Defer to the shared [`AccountState::is_initialized`] predicate so this
        // guard can't drift from it. This must be explicit now that
        // `authorize_actor` is an upsert and no longer reverts on a duplicate
        // initial actor (mirrors `createAccount`'s guard).
        let mut state = storage.get_account_state(address)?;
        if state.is_initialized() {
            return Err(ApplyError::AlreadyCreated { account: address });
        }

        // Mirror `createAccount`'s `CREATE2` deploy, which returns `address(0)`
        // (→ `AccountDeploymentFailed`) for runtime code over the EIP-170 cap or
        // leading with the EIP-3541 reserved `0xEF` byte. This path deploys via
        // `set_code(Bytecode::new_raw(..))` instead of `CREATE2`, so it enforces
        // both here — a leading-`0xEF` payload would otherwise panic in
        // `Bytecode::new_raw`. Checked after the already-created guard so a
        // collision still reports `AlreadyCreated` first, and before any state
        // write so a rejected create leaves no partially-initialized account.
        if entry.code.len() > Eip8130Constants::MAX_CODE_SIZE || entry.code.first() == Some(&0xEF) {
            return Err(ApplyError::AccountDeploymentFailed { account: address });
        }

        // Mark initialized, disable the implicit default-EOA path by default
        // (a created account has contract code, so the recovered==account path is
        // unreachable), and flag the account keystore-established so a later empty-
        // code state (e.g. an EIP-6780 SELFDESTRUCT) is never mistaken for a
        // proven-key EOA. Mirrors `createAccount`'s
        // `flags = FLAG_REVOKE_DEFAULT_EOA | FLAG_CONTRACT_ESTABLISHED`. Written
        // before initializing actors so a self-actorId k1 initial actor can
        // re-enable the inline self.
        state.local_sequence = 1;
        state.flags =
            Eip8130Constants::DEFAULT_EOA_REVOKED | Eip8130Constants::FLAG_CONTRACT_ESTABLISHED;
        storage.set_account_state(address, state)?;

        Self::initialize_actors(storage, address, &entry.initial_actors)?;
        // After initial actors (each already emitted `ActorAuthorized`), mirror
        // `createAccount`'s trailing `AccountCreated` log.
        AccountConfigurationEvents::emit_account_created(
            storage,
            address,
            entry.user_salt,
            keccak256(&entry.code),
        )?;

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
                return Err(ApplyError::ActorsNotSortedOrDuplicate);
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

    /// Decodes an `AuthorizeActor` op's `payload` into
    /// `(actorId, ActorConfig, policyData)`. Mirrors
    /// `abi.decode(payload, (bytes32, ActorConfig, bytes))`.
    ///
    /// Uses the *validating* decoder so dirty padding in the sub-256-bit
    /// `ActorConfig` fields (`scope` is `uint16`, `expiry` is `uint48`) is
    /// rejected rather than silently truncated. Solidity's `abi.decode` reverts
    /// on non-zero padding, so the lenient decoder would otherwise let the native
    /// path accept a signed payload the contract rejects — a consensus divergence.
    fn decode_authorize(payload: &[u8]) -> Result<(B256, ActorConfig, Bytes), ApplyError> {
        let (actor_id, abi, policy_data) =
            <(B256, ActorConfigAbi, Bytes)>::abi_decode_params_validate(payload)
                .map_err(|_| ApplyError::MalformedAuthorizeData)?;
        let config = ActorConfig {
            authenticator: abi.authenticator,
            scope: abi.scope,
            expiry: abi.expiry.to::<u64>(),
        };
        Ok((actor_id, config, policy_data))
    }

    /// Decodes a `RevokeActor` op's `payload` into its `actorId`. Mirrors
    /// `abi.decode(payload, (bytes32))`. Uses the validating decoder for parity
    /// with Solidity `abi.decode` (rejects trailing/misaligned bytes).
    fn decode_revoke(payload: &[u8]) -> Result<B256, ApplyError> {
        <(B256,)>::abi_decode_params_validate(payload)
            .map(|(actor_id,)| actor_id)
            .map_err(|_| ApplyError::MalformedRevokeData)
    }

    /// Validates `policy_data` against `scope`, returning `(manager,
    /// commitment)`. Mirrors `_slicePolicy`: an actor without `SCOPE_POLICY`
    /// requires empty data; a gated actor requires exactly
    /// `manager(20) || commitment(32)`, written verbatim. Neither field need be
    /// nonzero — a zero `commitment` is a valid "no parameters" value and a zero
    /// `manager` gates the key to `address(0)` (no productive target).
    pub fn slice_policy(scope: u16, policy_data: &[u8]) -> Result<(Address, B256), ApplyError> {
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

    /// The commitment over the initial actor set, using the hash-the-leaves-
    /// then-hash-the-list scheme shared with the signed digests. Each actor
    /// hashes to a fixed-width 32-byte leaf
    /// `keccak256(actorId(32) || authenticator(20) || scope(2) || policyData)`
    /// (`policyData` is empty for a non-policy actor, or exactly `manager(20) ||
    /// commitment(32)` when `POLICY` is set; `expiry` does not participate), and
    /// the commitment is `keccak256(leaf_0 || … || leaf_{n-1})`. Fixed-width
    /// leaves make the commitment unambiguous by construction and linear in the
    /// actor count. Mirrors `_computeActorsCommitment`, whose `scope` field is a
    /// `uint16` packed as 2 big-endian bytes.
    fn actors_commitment(initial_actors: &[InitialActor]) -> B256 {
        let mut packed_leaves = Vec::with_capacity(initial_actors.len() * 32);
        for actor in initial_actors {
            let mut leaf = Vec::with_capacity(54 + actor.policy_data.len());
            leaf.extend_from_slice(actor.actor_id.as_slice());
            leaf.extend_from_slice(actor.authenticator.as_slice());
            leaf.extend_from_slice(&actor.scope.to_be_bytes());
            leaf.extend_from_slice(&actor.policy_data);
            packed_leaves.extend_from_slice(keccak256(&leaf).as_slice());
        }
        keccak256(packed_leaves)
    }

    /// Builds an account's deployment code: a 14-byte EVM loader header that
    /// returns the trailing `bytecode` as the account's runtime code. Mirrors
    /// `_buildDeploymentCode`: rejects empty `bytecode` with
    /// [`ApplyError::EmptyBytecode`] (codeless creates are invalid) and bytecode
    /// over `0xFFFF` bytes with [`ApplyError::BytecodeTooLarge`].
    pub fn build_deployment_code(bytecode: &[u8]) -> Result<Vec<u8>, ApplyError> {
        let n = bytecode.len();
        if n == 0 {
            return Err(ApplyError::EmptyBytecode);
        }
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
    use alloy_primitives::{LogData, U256, address, b256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{HashMapStorageProvider, PrecompileStorageProvider, StorageCtx};
    use revm::state::Bytecode;

    use super::*;
    use crate::{AccountCreated, ActorAuthorized, ActorRevoked, DelegationApplied};

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

    /// Runs `body` against a fresh provider and returns the Account Configuration logs.
    fn with_storage_events(
        body: impl FnOnce(&mut AccountConfigurationStorage<'_>),
    ) -> Vec<LogData> {
        let mut provider = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut provider, |ctx| {
            body(&mut AccountConfigurationStorage::new(ctx));
        });
        provider.get_events(AccountConfigurationStorage::ADDRESS).clone()
    }

    /// `abi.encode(bytes32 actorId, ActorConfig, bytes policyData)` for an
    /// `AuthorizeActor` op payload.
    fn authorize_payload(actor_id: B256, config: &ActorConfig, policy_data: &[u8]) -> Bytes {
        let abi = ActorConfigAbi {
            authenticator: config.authenticator,
            scope: config.scope,
            expiry: alloy_primitives::aliases::U48::from(config.expiry),
        };
        Bytes::from((actor_id, abi, Bytes::copy_from_slice(policy_data)).abi_encode_params())
    }

    /// An `AuthorizeActor` [`SignedChange`] op.
    fn authorize_op(actor_id: B256, config: &ActorConfig, policy_data: &[u8]) -> SignedChange {
        SignedChange {
            change_type: ChangeType::AuthorizeActor,
            payload: authorize_payload(actor_id, config, policy_data),
        }
    }

    /// A `RevokeActor` [`SignedChange`] op (payload `abi.encode(bytes32 actorId)`).
    fn revoke_op(actor_id: B256) -> SignedChange {
        SignedChange {
            change_type: ChangeType::RevokeActor,
            payload: Bytes::from((actor_id,).abi_encode_params()),
        }
    }

    fn ungated(authenticator: Address, scope: u16) -> ActorConfig {
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
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
            assert!(acc.is_actor(ACCOUNT, NON_SELF).unwrap());

            // Upsert: re-authorizing an occupied slot overwrites it in place.
            let rescoped = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SELF_PAYER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, rescoped, &[]).unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), rescoped);

            // Revoke clears the slot.
            AccountChangeApplier::revoke_actor(acc, ACCOUNT, NON_SELF).unwrap();
            assert!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap().is_empty());
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
            assert!(acc.actor_config_slot(ACCOUNT, self_id).unwrap().is_empty());

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
            assert_eq!(acc.actor_config_slot(ACCOUNT, self_id).unwrap(), config);
        });
    }

    #[test]
    fn apply_config_change_counts_empty_revoke_slots() {
        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);

            // Revoking the live *ungated* inline k1 self (empty actor_config + both
            // empty policy slots) discounts all three conservatively-reset slots.
            let revoke_self = vec![revoke_op(self_id)];
            let count = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &revoke_self,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();
            assert_eq!(count, 3);
        });

        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
            // A *policy-gated* inline k1 self with both policy slots written
            // non-zero: only its empty actor_config slot is discounted; the two
            // real policy resets stay at the conservative price.
            let gated =
                ActorConfig { authenticator: K1, scope: Eip8130Constants::SCOPE_POLICY, expiry: 0 };
            let mut policy = Vec::new();
            policy.extend_from_slice(MANAGER.as_slice());
            policy.extend_from_slice(COMMITMENT.as_slice());
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, gated, &policy).unwrap();
            let revoke_self = vec![revoke_op(self_id)];
            let count = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &revoke_self,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();
            assert_eq!(count, 1);
        });

        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
            // A policy-gated inline k1 self may still carry *zero* policy slots
            // (the EIP permits a zero manager and/or commitment). Those slots are
            // zero-to-zero no-ops, so all three conservatively-reset slots are
            // discounted — the gate bit alone must not force a `1`.
            let gated =
                ActorConfig { authenticator: K1, scope: Eip8130Constants::SCOPE_POLICY, expiry: 0 };
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, gated, &[0u8; 52])
                .unwrap();
            let revoke_self = vec![revoke_op(self_id)];
            let count = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &revoke_self,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();
            assert_eq!(count, 3);
        });

        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
            // Mixed: a non-zero manager but a zero commitment. Only the empty
            // commitment slot (plus the always-empty actor_config) is discounted;
            // the written manager slot is a real reset.
            let gated =
                ActorConfig { authenticator: K1, scope: Eip8130Constants::SCOPE_POLICY, expiry: 0 };
            let mut policy = Vec::new();
            policy.extend_from_slice(MANAGER.as_slice());
            policy.extend_from_slice(B256::ZERO.as_slice());
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, gated, &policy).unwrap();
            let revoke_self = vec![revoke_op(self_id)];
            let count = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &revoke_self,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();
            assert_eq!(count, 2);
        });

        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
            // Install an *explicit* non-k1 self, then revoke it: this clears a
            // populated actor_config home, so it is not the discounted shape.
            let config = ungated(AUTHENTICATOR, 0);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, config, &[]).unwrap();
            let revoke_self = vec![revoke_op(self_id)];
            let count = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &revoke_self,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();
            assert_eq!(count, 0);
        });

        with_storage(|acc| {
            // Revoking a non-self actor is never the inline-self shape.
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SENDER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, config, &[]).unwrap();
            let revoke_other = vec![revoke_op(NON_SELF)];
            let count = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &revoke_other,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();
            assert_eq!(count, 0);
        });
    }

    #[test]
    fn config_change_advances_sequence_and_applies() {
        with_storage(|acc| {
            // Authorize a non-self actor in one multichain batch.
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SENDER);
            let changes = vec![authorize_op(NON_SELF, &config, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &changes,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();
            assert_eq!(acc.get_change_sequences(ACCOUNT).unwrap(), (1, 0));
            assert!(acc.is_actor(ACCOUNT, NON_SELF).unwrap());

            // A local-channel batch advances the local sequence instead. The
            // batch must be non-empty (the apply path rejects `EmptyChangeSet`),
            // so it carries a benign upsert whose actor set is not asserted here.
            let local_changes =
                vec![authorize_op(NON_SELF, &ungated(K1, Eip8130Constants::SCOPE_SENDER), &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &local_changes,
                AccountChangeChannel::Local,
                0,
            )
            .unwrap();
            assert_eq!(acc.get_change_sequences(ACCOUNT).unwrap(), (1, 1));
        });
    }

    /// An `IncrementLocalEpoch` op (empty payload).
    fn increment_epoch_op() -> SignedChange {
        SignedChange { change_type: ChangeType::IncrementLocalEpoch, payload: Bytes::new() }
    }

    #[test]
    fn increment_local_epoch_bumps_epoch_and_resets_sequence() {
        with_storage(|acc| {
            // Seed a local sequence of 4; a Local sequenced batch at seq 4 advances
            // it to 5, and the trailing IncrementLocalEpoch resets it to 0 while
            // bumping the epoch to 1.
            let mut state = acc.get_account_state(ACCOUNT).unwrap();
            state.local_sequence = 4;
            acc.set_account_state(ACCOUNT, state).unwrap();

            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[increment_epoch_op()],
                AccountChangeChannel::Local,
                4,
            )
            .unwrap();

            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert_eq!(state.local_epoch, 1);
            assert_eq!(state.local_sequence, 0);
        });
    }

    #[test]
    fn increment_local_epoch_on_multichain_channel_bumps_epoch() {
        with_storage(|acc| {
            // A Multichain batch may bump the local epoch; its own counter advances
            // independently.
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[increment_epoch_op()],
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();

            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert_eq!(state.local_epoch, 1);
            assert_eq!(state.multichain_sequence, 1);
            assert_eq!(state.local_sequence, 0);
        });
    }

    #[test]
    fn increment_local_epoch_rejects_nonempty_payload() {
        let mut state = AccountState::from_word(alloy_primitives::U256::ZERO);
        assert_eq!(
            AccountChangeApplier::apply_increment_local_epoch(&[0xaa], &mut state),
            Err(ApplyError::InvalidChangePayload),
        );
    }

    #[test]
    fn increment_local_epoch_rejects_saturated_epoch() {
        let mut state = AccountState::from_word(alloy_primitives::U256::ZERO);
        state.local_epoch = u64::from(u32::MAX);
        assert_eq!(
            AccountChangeApplier::apply_increment_local_epoch(&[], &mut state),
            Err(ApplyError::EpochSaturated),
        );
    }

    #[test]
    fn config_change_preserves_evolving_inline_self_state() {
        with_storage(|acc| {
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
            let scoped = ungated(K1, Eip8130Constants::SCOPE_SENDER);
            let changes = vec![authorize_op(self_id, &scoped, &[]), revoke_op(self_id)];

            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &changes,
                AccountChangeChannel::Multichain,
                0,
            )
            .unwrap();

            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert_eq!(state.multichain_sequence, 1);
            assert_eq!(state.local_sequence, 0);
            assert!(state.default_eoa_revoked());
            assert_eq!(state.default_eoa_scope, 0);
            assert_eq!(state.default_eoa_expiry, 0);
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
        assert_eq!(
            AccountChangeApplier::build_deployment_code(&[]),
            Err(ApplyError::EmptyBytecode)
        );
        assert_eq!(
            AccountChangeApplier::build_deployment_code(&vec![0u8; 0x10000]),
            Err(ApplyError::BytecodeTooLarge)
        );
    }

    #[test]
    fn actors_commitment_hashes_leaves_then_list() {
        // Golden values cross-checked against the contract's leaves-then-list
        // scheme (#74) with `cast keccak`: each actor hashes to
        // `keccak256(actorId(32) || authenticator(20) || scope(2 BE) ||
        // policyData)` and the commitment is `keccak256(leaf_0 || … ||
        // leaf_{n-1})`.
        let auth = address!("0x0000000000000000000000000000000000000001");
        let a1 = InitialActor::owner(B256::repeat_byte(0x11), auth);
        assert_eq!(
            AccountChangeApplier::actors_commitment(std::slice::from_ref(&a1)),
            b256!("0x072d109643fa2cb02a4727255a5d6f23a248f8e331c5be883d093115dd513ac9"),
        );

        let a2 = InitialActor {
            actor_id: B256::repeat_byte(0x22),
            authenticator: auth,
            scope: 0x0007,
            policy_data: Bytes::new(),
        };
        assert_eq!(
            AccountChangeApplier::actors_commitment(&[a1, a2]),
            b256!("0x7e3212d8f983663f5da75deeced0c37781e972611a5eec67cda3ed154c25affd"),
        );
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
                acc.actor_config_slot(expected, NON_SELF).unwrap(),
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
    fn create_rejected_when_account_has_only_local_epoch_state() {
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: b256!(
                    "0x4444444444444444444444444444444444444444444444444444444444444444"
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

            // An account whose local sequence was reset to 0 by IncrementLocalEpoch
            // still holds state: local_sequence == 0 && multichain_sequence == 0 but
            // local_epoch != 0. The guard must treat it as initialized (mirrors
            // `AccountState::is_initialized`), not re-creatable.
            let mut state = acc.get_account_state(expected).unwrap();
            state.local_epoch = 1;
            acc.set_account_state(expected, state).unwrap();

            assert_eq!(
                AccountChangeApplier::apply_create(acc, &entry),
                Err(ApplyError::AlreadyCreated { account: expected })
            );
        });
    }

    #[test]
    fn create_requires_sorted_non_empty_actors() {
        // Non-empty code so these entries reach the actor-set checks; the
        // finalized contract builds deployment code (reverting EmptyBytecode)
        // before `_initializeAccount`, so a codeless entry would fail earlier
        // (covered by `create_rejects_codeless_account`).
        let code = Bytes::from_static(&[0x60, 0x01]);
        with_storage(|acc| {
            let empty =
                CreateEntry { user_salt: B256::ZERO, code: code.clone(), initial_actors: vec![] };
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &empty),
                Err(ApplyError::NoInitialActors)
            );

            let unsorted = CreateEntry {
                user_salt: B256::ZERO,
                code: code.clone(),
                initial_actors: vec![
                    InitialActor::owner(B256::repeat_byte(2), AUTHENTICATOR),
                    InitialActor::owner(B256::repeat_byte(1), AUTHENTICATOR),
                ],
            };
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &unsorted),
                Err(ApplyError::ActorsNotSortedOrDuplicate)
            );
        });
    }

    #[test]
    fn create_rejects_codeless_account() {
        // Codeless creates are invalid: an account with actor config but no
        // runtime code (nor a delegation) would break the EOA invariant. Mirrors
        // `_buildDeploymentCode`'s `revert EmptyBytecode()`, which fires before
        // any actor-set validation, so even a well-formed actor set is rejected.
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: B256::ZERO,
                code: Bytes::new(),
                initial_actors: vec![InitialActor::owner(B256::repeat_byte(1), AUTHENTICATOR)],
            };
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &entry),
                Err(ApplyError::EmptyBytecode)
            );
        });
    }

    #[test]
    fn create_rejects_undeployable_runtime_code() {
        // The enshrined deploy (`set_code(Bytecode::new_raw(..))`) replaces the
        // contract's `CREATE2`, so it must reject the same payloads `CREATE2`
        // would fail with `address(0)`: EIP-3541 (leading `0xEF`) and EIP-170
        // (over `MAX_CODE_SIZE`). A leading-`0xEF` payload would otherwise panic
        // in `Bytecode::new_raw`.
        let actors = vec![InitialActor::owner(B256::repeat_byte(1), AUTHENTICATOR)];

        // EIP-3541: a leading `0xEF` byte is rejected as runtime code.
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: B256::ZERO,
                code: Bytes::from_static(&[0xEF, 0x00]),
                initial_actors: actors.clone(),
            };
            let expected = AccountChangeApplier::compute_address(
                entry.user_salt,
                &entry.code,
                &entry.initial_actors,
            )
            .unwrap();
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &entry),
                Err(ApplyError::AccountDeploymentFailed { account: expected })
            );
            // No partially-initialized account is left behind on rejection.
            assert!(!acc.get_account_state(expected).unwrap().is_initialized());
        });

        // EIP-170: runtime code over `MAX_CODE_SIZE` (but under the 0xFFFF
        // deployment-code cap) computes an address but is not deployable.
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: B256::ZERO,
                code: Bytes::from(vec![0x00u8; Eip8130Constants::MAX_CODE_SIZE + 1]),
                initial_actors: actors.clone(),
            };
            let expected = AccountChangeApplier::compute_address(
                entry.user_salt,
                &entry.code,
                &entry.initial_actors,
            )
            .unwrap();
            assert_eq!(
                AccountChangeApplier::apply_create(acc, &entry),
                Err(ApplyError::AccountDeploymentFailed { account: expected })
            );
        });

        // Exactly `MAX_CODE_SIZE` deploys (boundary, non-`0xEF` lead).
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: B256::ZERO,
                code: Bytes::from(vec![0x00u8; Eip8130Constants::MAX_CODE_SIZE]),
                initial_actors: actors.clone(),
            };
            assert!(AccountChangeApplier::apply_create(acc, &entry).is_ok());
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

    /// Marks `account` keystore-established (`FLAG_CONTRACT_ESTABLISHED`) in the
    /// `AccountConfiguration` storage, leaving every other state field zero.
    fn mark_contract_established(storage: &mut HashMapStorageProvider) {
        StorageCtx::enter(storage, |sctx| {
            let mut cfg = AccountConfigurationStorage::new(sctx);
            let mut state = AccountState::from_word(U256::ZERO);
            state.flags = Eip8130Constants::FLAG_CONTRACT_ESTABLISHED;
            cfg.set_account_state(ACCOUNT, state).unwrap();
        });
    }

    #[test]
    fn apply_create_flags_account_contract_established() {
        // A created account is marked keystore-established so a later empty-code
        // state can never be mistaken for a proven-key EOA. Mirrors
        // `createAccount`'s `FLAG_REVOKE_DEFAULT_EOA | FLAG_CONTRACT_ESTABLISHED`.
        let signer = address!("0x00000000000000000000000000000000000000a1");
        let actor_id = AccountConfigurationStorage::self_actor_id(signer);
        let entry = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: vec![InitialActor::owner(actor_id, Eip8130Constants::K1_AUTHENTICATOR)],
        };
        with_storage(|acc| {
            let created = AccountChangeApplier::apply_create(acc, &entry).unwrap();
            let state = acc.get_account_state(created.address).unwrap();
            assert!(state.contract_established(), "create must set FLAG_CONTRACT_ESTABLISHED");
            assert!(state.default_eoa_revoked(), "create must still revoke the default EOA");
        });
    }

    #[test]
    fn delegation_effect_install_rejects_empty_code_contract_established_account() {
        // Empty code on a keystore-established account is a self-destructed CREATE2
        // account, not a proven-key EOA — a delegation onto it is rejected.
        let target = Address::repeat_byte(0x55);
        let mut storage = HashMapStorageProvider::new(1);
        mark_contract_established(&mut storage);

        let error = StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, target).install(sctx)
        })
        .unwrap_err();

        assert_eq!(error, ApplyError::ContractEstablishedCodeless { account: ACCOUNT });
        // No delegation code was written.
        assert!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .and_then(Bytecode::eip7702_address)
                .is_none()
        );
    }

    #[test]
    fn delegation_effect_install_allows_empty_code_non_established_account() {
        // A genuine (non-established) empty-code EOA may still be delegated: the
        // guard only fires for keystore-established accounts.
        let target = Address::repeat_byte(0x56);
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
    fn delegation_effect_install_allows_redelegation_of_established_delegate() {
        // An established account that already carries a delegation indicator
        // (e.g. an imported 7702 delegate — genuinely once an EOA) may be
        // re-delegated: the codeless guard is scoped to *empty* code only.
        let target = Address::repeat_byte(0x57);
        let mut storage = HashMapStorageProvider::new(1);
        mark_contract_established(&mut storage);
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
    fn authorize_and_revoke_emit_protocol_logs() {
        let events = with_storage_events(|acc| {
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SENDER);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, config, &[]).unwrap();
            AccountChangeApplier::revoke_actor(acc, ACCOUNT, NON_SELF).unwrap();
        });
        assert_eq!(events.len(), 2);

        let authorized = ActorAuthorized::decode_log_data(&events[0]).unwrap();
        assert_eq!(authorized.account, ACCOUNT);
        assert_eq!(authorized.actorId, NON_SELF);
        assert_eq!(
            authorized.actorData,
            AccountConfigurationEvents::pack_actor_data(
                &ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_SENDER),
                Address::ZERO,
                B256::ZERO,
            )
        );

        let revoked = ActorRevoked::decode_log_data(&events[1]).unwrap();
        assert_eq!(revoked.account, ACCOUNT);
        assert_eq!(revoked.actorId, NON_SELF);
    }

    #[test]
    fn authorize_policy_actor_emits_84_byte_actor_data() {
        let mut policy = Vec::new();
        policy.extend_from_slice(MANAGER.as_slice());
        policy.extend_from_slice(COMMITMENT.as_slice());
        let config = ActorConfig {
            authenticator: AUTHENTICATOR,
            scope: Eip8130Constants::SCOPE_POLICY,
            expiry: 0,
        };

        let events = with_storage_events(|acc| {
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, config, &policy).unwrap();
        });
        assert_eq!(events.len(), 1);
        let authorized = ActorAuthorized::decode_log_data(&events[0]).unwrap();
        assert_eq!(authorized.actorData.len(), 84);
        assert_eq!(
            authorized.actorData,
            AccountConfigurationEvents::pack_actor_data(&config, MANAGER, COMMITMENT)
        );
    }

    #[test]
    fn create_emits_actor_authorized_then_account_created() {
        let entry = CreateEntry {
            user_salt: b256!("0x2222222222222222222222222222222222222222222222222222222222222222"),
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: vec![InitialActor::owner(NON_SELF, AUTHENTICATOR)],
        };
        let expected = AccountChangeApplier::compute_address(
            entry.user_salt,
            &entry.code,
            &entry.initial_actors,
        )
        .unwrap();

        let events = with_storage_events(|acc| {
            AccountChangeApplier::apply_create(acc, &entry).unwrap();
        });
        assert_eq!(events.len(), 2);

        let authorized = ActorAuthorized::decode_log_data(&events[0]).unwrap();
        assert_eq!(authorized.account, expected);
        assert_eq!(authorized.actorId, NON_SELF);

        let created = AccountCreated::decode_log_data(&events[1]).unwrap();
        assert_eq!(created.account, expected);
        assert_eq!(created.userSalt, entry.user_salt);
        assert_eq!(created.codeHash, keccak256(&entry.code));
    }

    #[test]
    fn delegation_install_emits_delegation_applied() {
        let target = Address::repeat_byte(0x33);
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, target).install(sctx)
        })
        .unwrap();

        let events = storage.get_events(AccountConfigurationStorage::ADDRESS);
        assert_eq!(events.len(), 1);
        let applied = DelegationApplied::decode_log_data(&events[0]).unwrap();
        assert_eq!(applied.account, ACCOUNT);
        assert_eq!(applied.target, target);
    }
}
