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

    /// A signed batch carried an unrecognized or not-yet-enshrined change type
    /// (currently `Lock` / `Unlock`, whose apply handlers are wired by a
    /// subsequent change). Mirrors `Keystore.UnknownChangeType`: rejected rather
    /// than silently ignored.
    #[error("unknown account-change op in the enshrined apply path")]
    UnknownChangeType,

    /// The operation is not permitted while the account is locked. Mirrors
    /// `Keystore.AccountIsLocked`.
    #[error("account is locked")]
    AccountIsLocked,

    /// An `AuthorizeActor` while locked carried an expiry that does not outlive
    /// the unlock floor. Mirrors `Keystore.ExpiryDoesNotOutliveUnlock`.
    #[error("authorize expiry does not outlive the unlock floor")]
    ExpiryDoesNotOutliveUnlock,

    /// A signed batch carried no changes. Mirrors `applySignedAccountChanges`'s
    /// `revert EmptyChangeSet()`: an empty batch would otherwise consume (advance)
    /// a channel's sequence without altering any configuration. Rejected before
    /// the sequence is advanced.
    #[error("signed account-change batch is empty")]
    EmptyChangeSet,

    /// The target `actor_id` is `bytes32(0)`, which is reserved for the "no
    /// actor" sentinel and can never be authorized. Mirrors `_authorizeActor`'s
    /// `revert InvalidActorId()`.
    #[error("actor id bytes32(0) is reserved and cannot be authorized")]
    InvalidActorId,

    /// The new actor's authenticator is `address(0)`, below the valid
    /// authenticator namespace. Mirrors `require(config.authenticator >= K1)`.
    #[error("authenticator address(0) is not a valid selector")]
    InvalidAuthenticator,

    /// `policyData` was neither empty nor exactly `manager(20) || commitment(32)`.
    /// Attachment is length-based (base/eip-8130 #95) and decoupled from
    /// `SCOPE_POLICY`. Mirrors `_slicePolicy`.
    #[error("policy data does not match policy type")]
    InvalidPolicyData,

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

    /// A create entry's bytecode exceeds the 0xFFFF deployment-loader limit.
    /// Mirrors `_buildDeploymentCode`'s `require(n <= 0xFFFF)` (the PUSH2 loader
    /// bound). The stricter [`Self::CreateCodeExceedsMaxSize`] EIP-170 cap is
    /// enforced first on the create path.
    #[error("create bytecode exceeds the 65535-byte limit")]
    BytecodeTooLarge,

    /// A create entry's runtime bytecode exceeds EIP-170's `MAX_CODE_SIZE`
    /// (24576). The reference contract deploys the runtime with a real `CREATE2`,
    /// whose returned code is subject to EIP-170; the enshrined path installs the
    /// runtime directly with `set_code`, so this bound must be enforced here or
    /// an inclusion path that bypasses mempool admission would install
    /// oversized code the reference implementation would reject.
    #[error("create bytecode exceeds the EIP-170 MAX_CODE_SIZE limit")]
    CreateCodeExceedsMaxSize,

    /// A create entry's runtime bytecode begins with the `0xEF` byte, which
    /// EIP-3541 forbids for deployed code. Real `CREATE2` in the reference
    /// contract rejects such runtimes; the enshrined path installs the runtime
    /// directly, so it must reject them too. This also prevents a `0xEF01`-
    /// prefixed runtime from reaching a panicking `Bytecode` constructor or from
    /// being silently reinterpreted as an EIP-7702 delegation designator.
    #[error("create bytecode begins with the EIP-3541-forbidden 0xEF byte")]
    CreateCodeStartsWithEf,

    /// The account targeted by a create entry already has EIP-8130 state. Mirrors
    /// the CREATE2 collision that makes `createAccount` unrepeatable.
    #[error("account {account} is already initialized")]
    AlreadyInitialized {
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

    /// A channel's sequence counter is at its terminal value and cannot advance.
    /// Mirrors `Keystore.SequenceSaturated`.
    #[error("account-change channel sequence is saturated")]
    SequenceSaturated,
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
    ///
    /// `now` is the block timestamp in Unix seconds (the same clock as
    /// [`ActorConfig::expiry`]), used to skip lapsed replayable JIT grants; see
    /// [`Self::apply_config_change_with_account_state`]. The read-only estimation
    /// pipeline passes `0` to price every change without filtering.
    pub fn apply_config_change(
        storage: &mut AccountConfigurationStorage<'_>,
        account: Address,
        changes: &[SignedChange],
        channel: AccountChangeChannel,
        sequence: u64,
        now: u64,
    ) -> Result<u32, ApplyError> {
        let mut state = storage.get_account_state(account)?;
        let revoke_discount_slots = Self::apply_config_change_with_account_state(
            storage, account, changes, channel, sequence, &mut state, now,
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
    /// Mirrors Keystore `_applyAuthorize`'s per-change skip of an already-expired
    /// grant on the replayable unsequenced (JIT) path: a JIT batch — a
    /// [`AccountChangeChannel::Local`] batch whose low sequence half is
    /// [`Eip8130Constants::UNSEQUENCED`] — consumes no counter and is replayable
    /// (last-write-wins on its slot until the epoch is incremented), so an
    /// `AuthorizeActor` grant whose non-zero expiry is not strictly in the future
    /// is silently **skipped** (dropped): a lapsed replayable grant can never
    /// re-land and clobber its slot, and whether a signed change applies never
    /// depends on onchain time. The skip is per-change — live siblings in the same
    /// batch still apply — and nothing reverts on expiry. Sequenced batches (local
    /// sequenced or any multichain) are single-consume and cannot be replayed, so
    /// an already-expired grant is retained and installs inert — present but dead
    /// on arrival at authentication — consuming its slot; multichain relies on this
    /// for cross-chain catch-up. A zero expiry is the "no expiry" sentinel and is
    /// always applied.
    ///
    /// `now` is the block timestamp in Unix seconds (the same clock as
    /// [`ActorConfig::expiry`]). Both the verifying and read-only estimation
    /// pipelines pass the real block timestamp so the JIT expiry skip is applied
    /// identically — the estimate's post-change state matches inclusion. Passing
    /// `0` disables the skip entirely (`expiry != 0 && expiry <= 0` is never
    /// true, so every grant is retained); callers that must price every change
    /// unconditionally, regardless of expiry, can opt into that with a `0` clock.
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
        now: u64,
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

        // A JIT batch (unsequenced Local) is replayable, so a lapsed grant is
        // skipped rather than installed; every other batch is single-consume and
        // retains an expired grant inert. Computed once for the whole batch.
        let is_unsequenced = matches!(channel, AccountChangeChannel::Local)
            && sequence as u32 == Eip8130Constants::UNSEQUENCED;
        let locked = state.is_locked(now);

        let mut revoke_discount_slots = 0u32;
        for change in changes {
            match change.change_type {
                ChangeType::AuthorizeActor => {
                    // Applies one `AuthorizeActor` op: JIT expiry skip, locked-account
                    // policy, then `_authorizeActor`. Mirrors `Keystore._applyAuthorize`.
                    let (actor_id, config, policy_data) = Self::decode_authorize(&change.payload)?;
                    // Replayable JIT path: drop an already-lapsed grant without
                    // reverting. Uses the canonical `_isExpired` boundary (`now >
                    // expiry`), so a grant with `expiry == now` is still live for
                    // that second and installs rather than being dropped here.
                    if is_unsequenced && config.is_expired(now) {
                        continue;
                    }
                    if locked {
                        Self::enforce_locked_authorize_rules(
                            storage,
                            account,
                            actor_id,
                            &config,
                            &policy_data,
                            state,
                            now,
                        )?;
                    }
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
                    if locked {
                        return Err(ApplyError::AccountIsLocked);
                    }
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
                    return Err(ApplyError::UnknownChangeType);
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
                        u64::from(seq).checked_add(1).ok_or(ApplyError::SequenceSaturated)?;
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
                state.multichain_sequence = state
                    .multichain_sequence
                    .checked_add(1)
                    .ok_or(ApplyError::SequenceSaturated)?;
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

    /// Locked-account guard for `AuthorizeActor`. Mirrors `Keystore._applyAuthorize`'s
    /// `locked` block:
    ///
    /// - **No live entry** ([`AccountConfigurationStorage::resolve_live_actor_config_with_state`]
    ///   empty — unknown, revoked, disabled, or expired): a new add is allowed once
    ///   the grant's expiry outlives the unlock floor (`expiry == 0` always passes).
    /// - **Live entry** (slot populated and not expired): only an expiry rewrite is
    ///   allowed; authenticator, scope, and policy (manager + commitment) must match
    ///   the stored values. The new expiry must still outlive the unlock floor.
    fn enforce_locked_authorize_rules(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        config: &ActorConfig,
        policy_data: &[u8],
        state: &AccountState,
        now: u64,
    ) -> Result<(), ApplyError> {
        let unlock_floor = state.unlock_floor(now);
        if config.expiry != 0 && config.expiry <= unlock_floor {
            return Err(ApplyError::ExpiryDoesNotOutliveUnlock);
        }

        // Resolve the current live config against the evolving in-batch `state`, not
        // persisted storage: an earlier op's inline-self rewrite lands in `state`
        // and is flushed only at batch end, so a storage read would miss it and let
        // a second self identity/scope rewrite slip past the live-entry guard.
        let current =
            storage.resolve_live_actor_config_with_state(account, actor_id, state, now)?;
        if current.is_empty() {
            return Ok(());
        }

        let (manager, commitment) = Self::slice_policy(policy_data)?;
        if config.authenticator != current.authenticator
            || config.scope != current.scope
            || manager != storage.get_policy_manager(account, actor_id)?
            || commitment != storage.get_policy_commitment(account, actor_id)?
        {
            return Err(ApplyError::AccountIsLocked);
        }
        Ok(())
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

        let (manager, commitment) = Self::slice_policy(policy_data)?;
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
            storage,
            account,
            actor_id,
            &config,
            manager,
            commitment,
            Self::policy_attached(policy_data),
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
        // Zero is reserved for the "no actor" sentinel. Mirrors `_authorizeActor`'s
        // `revert InvalidActorId()`. A zero `actor_id` only ever reaches the
        // non-self path (the self id is derived from a nonzero account address),
        // so guarding here covers every `AuthorizeActor`; the create path already
        // rejects it earlier via the strictly-ascending `initial_actors` check.
        if actor_id.is_zero() {
            return Err(ApplyError::InvalidActorId);
        }
        if config.authenticator.is_zero() {
            return Err(ApplyError::InvalidAuthenticator);
        }
        let (manager, commitment) = Self::slice_policy(policy_data)?;
        // Non-self actor: a single `actor_config` home. Upsert: overwrite in
        // place. Both policy slots are always touched so zero-to-zero clears
        // preserve the reference operation's access warming.
        storage.set_actor_config(account, actor_id, config)?;
        storage.set_policy(account, actor_id, manager, commitment)?;
        AccountConfigurationEvents::emit_actor_authorized(
            storage,
            account,
            actor_id,
            &config,
            manager,
            commitment,
            Self::policy_attached(policy_data),
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
    /// - An **absent** actor revoke is an idempotent no-op (mirrors
    ///   `_applyRevoke`'s early return, eip-8130 #100) and returns `3`: all three
    ///   conservatively reset-priced slots are empty, so the whole revoke is
    ///   discounted.
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
            // Idempotent: the actor is already absent — no explicit `actor_config`
            // entry, and either a non-self id or a self whose inline secp256k1 key
            // is revoked. Mirrors `_applyRevoke`'s `if (!_isAuthorized(...)) return;`
            // (eip-8130 #100): no state change and no `ActorRevoked` event. All
            // three conservatively reset-priced revoke slots (`actor_config` and
            // both policy slots) are empty for an absent actor, so the whole revoke
            // is discounted.
            return Ok(3);
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
            // Idempotent: an absent actor is a no-op (no clear, no event), mirroring
            // `_applyRevoke`'s early return when `!_isAuthorized` (eip-8130 #100).
            return Ok(());
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
        max_code_size: usize,
    ) -> Result<CreatedAccount, ApplyError> {
        // Validate the runtime before deriving the address so every caller (pool
        // admission and block inclusion) rejects the same set of malformed
        // runtimes at the shared choke point, matching the reference contract's
        // CREATE2 deploy (active code-size cap, EIP-3541 leading-byte).
        Self::validate_create_runtime(&entry.code, max_code_size)?;
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
            return Err(ApplyError::AlreadyInitialized { account: address });
        }

        // Mark initialized and disable the implicit default-EOA path by default
        // (a created account has contract code, so the recovered==account path is
        // unreachable). Mirrors `createAccount`'s `flags = FLAG_REVOKE_DEFAULT_EOA`.
        // Written before initializing actors so a self-actorId k1 initial actor can
        // re-enable the inline self.
        state.local_sequence = 1;
        state.flags = Eip8130Constants::DEFAULT_EOA_REVOKED;
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
    pub fn decode_authorize(payload: &[u8]) -> Result<(B256, ActorConfig, Bytes), ApplyError> {
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

    /// Validates and slices `policy_data` by **length**, returning `(manager,
    /// commitment)`. Mirrors the finalized `_slicePolicy` (base/eip-8130 #95):
    /// length decides what gets stored; POLICY decides whether the sender is
    /// gated; OPERATOR overrides POLICY. Empty data attaches no policy (both
    /// fields zero); exactly `manager(20) || commitment(32)` attaches the two,
    /// written verbatim (either may be zero — a zero `commitment` is a valid "no
    /// parameters" value and a zero `manager` is `address(0)`); any other length
    /// is rejected. The `SCOPE_POLICY` bit is the protocol sender gate and is
    /// deliberately not consulted here — use [`Self::policy_attached`] to learn
    /// whether a payload carried policy bytes.
    pub fn slice_policy(policy_data: &[u8]) -> Result<(Address, B256), ApplyError> {
        if policy_data.is_empty() {
            return Ok((Address::ZERO, B256::ZERO));
        }
        if policy_data.len() != Eip8130Constants::POLICY_DATA_LEN {
            return Err(ApplyError::InvalidPolicyData);
        }
        let manager = Address::from_slice(&policy_data[..20]);
        let commitment = B256::from_slice(&policy_data[20..Eip8130Constants::POLICY_DATA_LEN]);
        Ok((manager, commitment))
    }

    /// Whether an authorize payload's `policy_data` carries policy bytes, decided
    /// solely by length (non-empty ⇒ attached). Distinguishes a genuine all-zero
    /// policy attachment (`52` zero bytes) from no attachment at all, which the
    /// `(manager, commitment)` pair alone cannot express.
    #[must_use]
    pub const fn policy_attached(policy_data: &[u8]) -> bool {
        !policy_data.is_empty()
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

    /// Validates a create entry's runtime bytecode against the constraints the
    /// reference contract's real `CREATE2` deploy enforces, which the enshrined
    /// path — installing the runtime directly with `set_code` rather than
    /// deploying it — would otherwise skip:
    ///
    /// - non-empty ([`ApplyError::EmptyBytecode`]): a codeless create would leave
    ///   an account with actor config but no code, breaking the EOA invariant.
    /// - at most `max_code_size` ([`ApplyError::CreateCodeExceedsMaxSize`]), the
    ///   block's active deployed-code cap (EIP-170 pre-Amsterdam, EIP-7954 after),
    ///   matching the limit the EVM enforces for ordinary `CREATE`.
    /// - not `0xEF`-prefixed per EIP-3541 ([`ApplyError::CreateCodeStartsWithEf`]),
    ///   which also keeps a `0xEF01`-prefixed runtime away from the panicking
    ///   `Bytecode` designator constructor and from being reinterpreted as an
    ///   EIP-7702 delegation.
    pub fn validate_create_runtime(
        bytecode: &[u8],
        max_code_size: usize,
    ) -> Result<(), ApplyError> {
        if bytecode.is_empty() {
            return Err(ApplyError::EmptyBytecode);
        }
        if bytecode.len() > max_code_size {
            return Err(ApplyError::CreateCodeExceedsMaxSize);
        }
        if bytecode[0] == 0xEF {
            return Err(ApplyError::CreateCodeStartsWithEf);
        }
        Ok(())
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
    use alloy_primitives::{LogData, address, b256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{HashMapStorageProvider, PrecompileStorageProvider, StorageCtx};
    use revm::state::Bytecode;

    use super::*;
    use crate::{AccountCreated, ActorAuthorized, ActorRevoked, DelegationApplied};

    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000a1");
    /// Pre-Amsterdam (EIP-170) deployed-code cap used by the tests.
    const MAX_CODE: usize = Eip8130Constants::MAX_CODE_SIZE;
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

    fn expiring(expiry: u64) -> ActorConfig {
        ActorConfig { authenticator: AUTHENTICATOR, scope: 0, expiry }
    }

    fn set_hard_locked(acc: &mut AccountConfigurationStorage<'_>, delay: u64) {
        let mut state = acc.get_account_state(ACCOUNT).unwrap();
        state.flags = Eip8130Constants::FLAG_LOCKED;
        state.lock_union = delay;
        acc.set_account_state(ACCOUNT, state).unwrap();
    }

    #[test]
    fn locked_authorize_succeeds_when_expiry_outlives_hard_lock_floor() {
        let now = 1_000u64;
        let delay = 3_600u64;
        with_storage(|acc| {
            set_hard_locked(acc, delay);
            let config = expiring(now + delay + 1);
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &config, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
        });
    }

    #[test]
    fn locked_authorize_reverts_when_expiry_at_hard_lock_floor() {
        let now = 1_000u64;
        let delay = 3_600u64;
        with_storage(|acc| {
            set_hard_locked(acc, delay);
            let config = expiring(now + delay);
            let err = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &config, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap_err();
            assert_eq!(err, ApplyError::ExpiryDoesNotOutliveUnlock);
        });
    }

    #[test]
    fn locked_authorize_allows_unbounded_expiry() {
        let now = 1_000u64;
        with_storage(|acc| {
            set_hard_locked(acc, 3_600);
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR);
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &config, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
        });
    }

    #[test]
    fn locked_revoke_is_rejected() {
        let now = 1_000u64;
        with_storage(|acc| {
            AccountChangeApplier::authorize_actor(
                acc,
                ACCOUNT,
                NON_SELF,
                ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR),
                &[],
            )
            .unwrap();
            set_hard_locked(acc, 3_600);
            let err = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[revoke_op(NON_SELF)],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap_err();
            assert_eq!(err, ApplyError::AccountIsLocked);
        });
    }

    #[test]
    fn locked_reauthorize_allows_expiry_only_re_lease() {
        let now = 1_000u64;
        let delay = 3_600u64;
        with_storage(|acc| {
            AccountChangeApplier::authorize_actor(
                acc,
                ACCOUNT,
                NON_SELF,
                expiring(now + 86_400),
                &[],
            )
            .unwrap();
            set_hard_locked(acc, delay);
            let shorter = expiring(now + delay + 1);
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &shorter, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), shorter);
        });
    }

    #[test]
    fn locked_reauthorize_rejects_scope_change() {
        let now = 1_000u64;
        with_storage(|acc| {
            AccountChangeApplier::authorize_actor(
                acc,
                ACCOUNT,
                NON_SELF,
                expiring(now + 86_400),
                &[],
            )
            .unwrap();
            set_hard_locked(acc, 3_600);
            let widened = ActorConfig {
                authenticator: AUTHENTICATOR,
                scope: Eip8130Constants::SCOPE_OPERATOR,
                expiry: now + 86_400,
            };
            let err = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &widened, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap_err();
            assert_eq!(err, ApplyError::AccountIsLocked);
        });
    }

    #[test]
    fn locked_self_release_uses_evolving_state_within_batch() {
        // Two AuthorizeActor ops on the inline self in one locked batch: the first
        // is a fresh add (self starts revoked), the second rewrites the scope. The
        // second must be rejected as a live-entry identity change — the applier
        // resolves the self against the evolving in-batch `AccountState`, not the
        // stale persisted slot, so an add-then-mutate cannot slip past the lock.
        let now = 1_000u64;
        let delay = 3_600u64;
        let expiry = now + delay + 100; // outlives the hard-lock floor (now + delay)
        let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
        with_storage(|acc| {
            // Locked with the inline self revoked, so the first op is a new add.
            let mut state = acc.get_account_state(ACCOUNT).unwrap();
            state.flags = Eip8130Constants::FLAG_LOCKED | Eip8130Constants::DEFAULT_EOA_REVOKED;
            state.lock_union = delay;
            acc.set_account_state(ACCOUNT, state).unwrap();

            let add =
                ActorConfig { authenticator: K1, scope: Eip8130Constants::SCOPE_OPERATOR, expiry };
            let rescope = ActorConfig { authenticator: K1, scope: 0, expiry };
            let err = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(self_id, &add, &[]), authorize_op(self_id, &rescope, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap_err();
            assert_eq!(err, ApplyError::AccountIsLocked);
        });
    }

    #[test]
    fn locked_expired_actor_is_treated_as_new_add() {
        let now = 10_000u64;
        with_storage(|acc| {
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, expiring(now - 1), &[])
                .unwrap();
            set_hard_locked(acc, 3_600);
            let replacement = ActorConfig { authenticator: K1, scope: 0, expiry: now + 3_601 };
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &replacement, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), replacement);
        });
    }

    #[test]
    fn locked_authorize_reverts_when_expiry_at_pending_unlock_floor() {
        let now = 1_000u64;
        let delay = 3_600u64;
        with_storage(|acc| {
            let mut state = acc.get_account_state(ACCOUNT).unwrap();
            state.flags = Eip8130Constants::FLAG_LOCKED | Eip8130Constants::FLAG_UNLOCK_INITIATED;
            state.lock_union = now + delay;
            acc.set_account_state(ACCOUNT, state).unwrap();

            let config = expiring(now + delay);
            let err = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &config, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap_err();
            assert_eq!(err, ApplyError::ExpiryDoesNotOutliveUnlock);
        });
    }

    #[test]
    fn locked_authorize_succeeds_when_expiry_above_pending_unlock_floor() {
        let now = 1_000u64;
        let delay = 3_600u64;
        with_storage(|acc| {
            let mut state = acc.get_account_state(ACCOUNT).unwrap();
            state.flags = Eip8130Constants::FLAG_LOCKED | Eip8130Constants::FLAG_UNLOCK_INITIATED;
            state.lock_union = now + delay;
            acc.set_account_state(ACCOUNT, state).unwrap();

            let config = expiring(now + delay + 1);
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &[authorize_op(NON_SELF, &config, &[])],
                AccountChangeChannel::Local,
                0,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
        });
    }

    #[test]
    fn apply_skips_lapsed_jit_grant() {
        let now = 1_000u64;
        let jit = u64::from(Eip8130Constants::UNSEQUENCED);
        let unauthorized = ActorConfig { authenticator: Address::ZERO, scope: 0, expiry: 0 };
        // A strictly-past expiry on the replayable JIT path is skipped (dropped),
        // not reverted: the grant never lands in its slot.
        with_storage(|acc| {
            let past = [authorize_op(NON_SELF, &expiring(now - 1), &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &past,
                AccountChangeChannel::Local,
                jit,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), unauthorized);
        });
        // `expiry == now` is still live for that second (canonical `_isExpired`
        // is strict, `now > expiry`), so the JIT grant installs rather than being
        // dropped.
        with_storage(|acc| {
            let at_now = expiring(now);
            let ops = [authorize_op(NON_SELF, &at_now, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &ops,
                AccountChangeChannel::Local,
                jit,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), at_now);
        });
    }

    #[test]
    fn authorize_rejects_zero_actor_id() {
        // `bytes32(0)` is the reserved "no actor" sentinel; authorizing it must
        // revert `InvalidActorId`, mirroring the contract's `_authorizeActor`.
        with_storage(|acc| {
            let zero =
                [authorize_op(B256::ZERO, &ungated(K1, Eip8130Constants::SCOPE_OPERATOR), &[])];
            let err = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &zero,
                AccountChangeChannel::Local,
                0,
                0,
            )
            .unwrap_err();
            assert!(matches!(err, ApplyError::InvalidActorId));
        });
    }

    #[test]
    fn apply_keeps_future_and_zero_expiry_jit_grant() {
        let now = 1_000u64;
        let jit = u64::from(Eip8130Constants::UNSEQUENCED);
        // Strictly-future expiry lands verbatim on the JIT path.
        with_storage(|acc| {
            let config = expiring(now + 1);
            let future = [authorize_op(NON_SELF, &config, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &future,
                AccountChangeChannel::Local,
                jit,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
        });
        // Zero expiry is the "no expiry" sentinel and always lands.
        with_storage(|acc| {
            let config = ungated(AUTHENTICATOR, 0);
            let never = [authorize_op(NON_SELF, &config, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &never,
                AccountChangeChannel::Local,
                jit,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
        });
    }

    #[test]
    fn apply_jit_applies_live_siblings_only() {
        let now = 1_000u64;
        let jit = u64::from(Eip8130Constants::UNSEQUENCED);
        let other: B256 =
            b256!("0x00000000000000000000000000000000000000ee000000000000000000000000");
        let unauthorized = ActorConfig { authenticator: Address::ZERO, scope: 0, expiry: 0 };
        // A mixed JIT batch: the lapsed grant is skipped per-change; the live
        // sibling in the same batch still applies.
        with_storage(|acc| {
            let live = expiring(now + 1);
            let batch =
                [authorize_op(NON_SELF, &expiring(now - 1), &[]), authorize_op(other, &live, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &batch,
                AccountChangeChannel::Local,
                jit,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), unauthorized);
            assert_eq!(acc.actor_config_slot(ACCOUNT, other).unwrap(), live);
        });
    }

    #[test]
    fn apply_retains_expired_grant_when_not_jit() {
        let now = 1_000u64;
        let config = expiring(now - 1);
        // Sequenced Local batch (low half != UNSEQUENCED): single-consume, so an
        // expired grant is retained and installs inert rather than being skipped.
        with_storage(|acc| {
            let expired = [authorize_op(NON_SELF, &config, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &expired,
                AccountChangeChannel::Local,
                5,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
        });
        // Multichain is never JIT (needed for cross-chain catch-up), so an expired
        // grant likewise installs inert instead of being skipped.
        with_storage(|acc| {
            let expired = [authorize_op(NON_SELF, &config, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &expired,
                AccountChangeChannel::Multichain,
                0,
                now,
            )
            .unwrap();
            assert_eq!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap(), config);
        });
    }

    #[test]
    fn slice_policy_matches_contract() {
        // Length-based (base/eip-8130 #95), decoupled from scope: empty attaches
        // nothing; exactly 52 bytes attaches; any other length rejects.
        assert_eq!(AccountChangeApplier::slice_policy(&[]).unwrap(), (Address::ZERO, B256::ZERO));
        assert!(!AccountChangeApplier::policy_attached(&[]));
        assert_eq!(AccountChangeApplier::slice_policy(&[1]), Err(ApplyError::InvalidPolicyData));

        let mut data = Vec::new();
        data.extend_from_slice(MANAGER.as_slice());
        data.extend_from_slice(COMMITMENT.as_slice());
        assert_eq!(AccountChangeApplier::slice_policy(&data).unwrap(), (MANAGER, COMMITMENT));
        assert!(AccountChangeApplier::policy_attached(&data));

        // Wrong length rejects.
        assert_eq!(
            AccountChangeApplier::slice_policy(&data[..51]),
            Err(ApplyError::InvalidPolicyData)
        );
        // Neither field need be nonzero: a zero manager/commitment is well-formed
        // (`manager(20) || commitment(32)`), yet still counts as attached.
        let zero_mgr = [0u8; 52];
        assert_eq!(
            AccountChangeApplier::slice_policy(&zero_mgr).unwrap(),
            (Address::ZERO, B256::ZERO)
        );
        assert!(AccountChangeApplier::policy_attached(&zero_mgr));
    }

    #[test]
    fn authorize_and_revoke_non_self_actor() {
        with_storage(|acc| {
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR);
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
            // Revoking an already-absent actor is idempotent: a no-op, not an
            // error (mirrors `_applyRevoke`'s early return, eip-8130 #100).
            assert_eq!(AccountChangeApplier::revoke_actor(acc, ACCOUNT, NON_SELF), Ok(()));
            assert!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap().is_empty());
        });
    }

    #[test]
    fn revoke_absent_actor_is_idempotent_noop_with_full_discount() {
        // Revoking an actor that was never authorized mirrors `_applyRevoke`'s
        // `if (!_isAuthorized(...)) return;` (eip-8130 #100): a no-op that emits
        // no `ActorRevoked` event and discounts all three conservatively
        // reset-priced revoke slots (they are all empty for an absent actor).
        let events = with_storage_events(|acc| {
            let mut state = acc.get_account_state(ACCOUNT).unwrap();
            assert_eq!(
                AccountChangeApplier::revoke_actor_with_account_state(
                    acc, ACCOUNT, NON_SELF, &mut state
                ),
                Ok(3),
            );
            assert!(acc.actor_config_slot(ACCOUNT, NON_SELF).unwrap().is_empty());
        });
        assert!(events.is_empty(), "a no-op revoke must not emit ActorRevoked");
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
    fn policy_attachment_is_length_based_and_read_is_scope_gated() {
        // base/eip-8130 #95: length decides what gets stored; POLICY decides
        // whether the sender is gated; OPERATOR overrides POLICY. Attachment is
        // accepted for any 52-byte payload (decoupled from scope). The protocol
        // sender gate stays `SCOPE_POLICY`-based (not length-based).
        with_storage(|acc| {
            let mut data = Vec::new();
            data.extend_from_slice(MANAGER.as_slice());
            data.extend_from_slice(COMMITMENT.as_slice());

            // Wrong length is rejected regardless of scope.
            let unrestricted = ActorConfig { authenticator: AUTHENTICATOR, scope: 0, expiry: 0 };
            assert_eq!(
                AccountChangeApplier::authorize_actor(
                    acc,
                    ACCOUNT,
                    NON_SELF,
                    unrestricted,
                    &data[..51]
                ),
                Err(ApplyError::InvalidPolicyData)
            );

            // 52-byte policy with an ungated scope now *succeeds* (length-based)
            // and writes the raw slots, but the scope-gated read reports no policy.
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, unrestricted, &data)
                .unwrap();
            assert_eq!(acc.get_policy_manager(ACCOUNT, NON_SELF).unwrap(), MANAGER);
            assert_eq!(acc.get_policy(ACCOUNT, NON_SELF).unwrap(), (Address::ZERO, B256::ZERO));

            // SCOPE_POLICY actor: the same attachment now reads back as gated.
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
            // must be cleared. `set_policy` always writes both slots, so an empty
            // `policy_data` upsert writes zeros (length-based; independent of scope).
            let ungated_cfg = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR);
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
            let scoped = ungated(K1, Eip8130Constants::SCOPE_OPERATOR);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, self_id, scoped, &[]).unwrap();
            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert!(!state.default_eoa_revoked());
            assert_eq!(state.default_eoa_scope, Eip8130Constants::SCOPE_OPERATOR);
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
                0,
            )
            .unwrap();
            assert_eq!(count, 0);
        });

        with_storage(|acc| {
            // Revoking a non-self actor is never the inline-self shape.
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR);
            AccountChangeApplier::authorize_actor(acc, ACCOUNT, NON_SELF, config, &[]).unwrap();
            let revoke_other = vec![revoke_op(NON_SELF)];
            let count = AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &revoke_other,
                AccountChangeChannel::Multichain,
                0,
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
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR);
            let changes = vec![authorize_op(NON_SELF, &config, &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &changes,
                AccountChangeChannel::Multichain,
                0,
                0,
            )
            .unwrap();
            assert_eq!(acc.get_change_sequences(ACCOUNT).unwrap(), (1, 0));
            assert!(acc.is_actor(ACCOUNT, NON_SELF).unwrap());

            // A local-channel batch advances the local sequence instead. The
            // batch must be non-empty (the apply path rejects `EmptyChangeSet`),
            // so it carries a benign upsert whose actor set is not asserted here.
            let local_changes =
                vec![authorize_op(NON_SELF, &ungated(K1, Eip8130Constants::SCOPE_OPERATOR), &[])];
            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &local_changes,
                AccountChangeChannel::Local,
                0,
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
                0,
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
            let scoped = ungated(K1, Eip8130Constants::SCOPE_OPERATOR);
            let changes = vec![authorize_op(self_id, &scoped, &[]), revoke_op(self_id)];

            AccountChangeApplier::apply_config_change(
                acc,
                ACCOUNT,
                &changes,
                AccountChangeChannel::Multichain,
                0,
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
    fn validate_create_runtime_enforces_create2_deploy_rules() {
        const PRE: usize = Eip8130Constants::MAX_CODE_SIZE;
        const AMS: usize = Eip8130Constants::MAX_CODE_SIZE_AMSTERDAM;
        // Empty runtime is rejected (codeless create), regardless of the cap.
        assert_eq!(
            AccountChangeApplier::validate_create_runtime(&[], PRE),
            Err(ApplyError::EmptyBytecode)
        );
        // A runtime at exactly the active cap is allowed; one byte over is not.
        assert!(AccountChangeApplier::validate_create_runtime(&vec![0x00u8; PRE], PRE).is_ok());
        assert_eq!(
            AccountChangeApplier::validate_create_runtime(&vec![0x00u8; PRE + 1], PRE),
            Err(ApplyError::CreateCodeExceedsMaxSize)
        );
        // EIP-7954: under the Amsterdam cap a runtime between the old and new
        // limits is accepted, and the new boundary is enforced.
        assert!(AccountChangeApplier::validate_create_runtime(&vec![0x00u8; PRE + 1], AMS).is_ok());
        assert!(AccountChangeApplier::validate_create_runtime(&vec![0x00u8; AMS], AMS).is_ok());
        assert_eq!(
            AccountChangeApplier::validate_create_runtime(&vec![0x00u8; AMS + 1], AMS),
            Err(ApplyError::CreateCodeExceedsMaxSize)
        );
        // A runtime over the old cap is still rejected under the pre-Amsterdam cap.
        assert_eq!(
            AccountChangeApplier::validate_create_runtime(&vec![0x00u8; AMS], PRE),
            Err(ApplyError::CreateCodeExceedsMaxSize)
        );
        // EIP-3541: any runtime beginning with 0xEF is rejected, including the
        // 3-byte 0xEF0100 prefix that would otherwise panic the EIP-7702
        // designator constructor, and a full 0xEF0100||target designator.
        assert_eq!(
            AccountChangeApplier::validate_create_runtime(&[0xEF, 0x00], PRE),
            Err(ApplyError::CreateCodeStartsWithEf)
        );
        assert_eq!(
            AccountChangeApplier::validate_create_runtime(&[0xEF, 0x01, 0x00], PRE),
            Err(ApplyError::CreateCodeStartsWithEf)
        );
        // An ordinary runtime is accepted.
        assert!(AccountChangeApplier::validate_create_runtime(&[0x60, 0x00], PRE).is_ok());
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

            let created = AccountChangeApplier::apply_create(acc, &entry, MAX_CODE).unwrap();
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
                AccountChangeApplier::apply_create(acc, &entry, MAX_CODE),
                Err(ApplyError::AlreadyInitialized { account: expected })
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
                AccountChangeApplier::apply_create(acc, &entry, MAX_CODE),
                Err(ApplyError::AlreadyInitialized { account: expected })
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
                AccountChangeApplier::apply_create(acc, &entry, MAX_CODE),
                Err(ApplyError::AlreadyInitialized { account: expected })
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
                AccountChangeApplier::apply_create(acc, &empty, MAX_CODE),
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
                AccountChangeApplier::apply_create(acc, &unsorted, MAX_CODE),
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
                AccountChangeApplier::apply_create(acc, &entry, MAX_CODE),
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
                AccountChangeApplier::apply_create(acc, &entry, MAX_CODE),
                Err(ApplyError::CreateCodeStartsWithEf)
            );
            // No partially-initialized account is left behind on rejection.
            assert!(!acc.get_account_state(expected).unwrap().is_initialized());
        });

        // EIP-170: runtime code over `MAX_CODE_SIZE` (but under the 0xFFFF
        // deployment-code cap) is rejected before any state is written.
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
                AccountChangeApplier::apply_create(acc, &entry, MAX_CODE),
                Err(ApplyError::CreateCodeExceedsMaxSize)
            );
            assert!(!acc.get_account_state(expected).unwrap().is_initialized());
        });

        // Exactly `MAX_CODE_SIZE` deploys (boundary, non-`0xEF` lead).
        with_storage(|acc| {
            let entry = CreateEntry {
                user_salt: B256::ZERO,
                code: Bytes::from(vec![0x00u8; Eip8130Constants::MAX_CODE_SIZE]),
                initial_actors: actors.clone(),
            };
            assert!(AccountChangeApplier::apply_create(acc, &entry, MAX_CODE).is_ok());
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

    #[test]
    fn apply_create_flags_revokes_default_eoa() {
        // A created account disables the implicit default-EOA path. Mirrors
        // `createAccount`'s `flags = FLAG_REVOKE_DEFAULT_EOA`.
        let signer = address!("0x00000000000000000000000000000000000000a1");
        let actor_id = AccountConfigurationStorage::self_actor_id(signer);
        let entry = CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: vec![InitialActor::owner(actor_id, Eip8130Constants::K1_AUTHENTICATOR)],
        };
        with_storage(|acc| {
            let created = AccountChangeApplier::apply_create(acc, &entry, MAX_CODE).unwrap();
            let state = acc.get_account_state(created.address).unwrap();
            assert!(state.default_eoa_revoked(), "create must revoke the default EOA");
        });
    }

    #[test]
    fn delegation_effect_install_allows_empty_code_account() {
        // An empty-code account may be delegated: the only code-shape guard is
        // that ordinary contract bytecode is rejected.
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
    fn delegation_effect_install_allows_redelegation_of_existing_delegate() {
        // An account that already carries a delegation indicator may be
        // re-delegated: existing delegation code is replaceable.
        let target = Address::repeat_byte(0x57);
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
    fn authorize_and_revoke_emit_protocol_logs() {
        let events = with_storage_events(|acc| {
            let config = ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR);
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
                &ungated(AUTHENTICATOR, Eip8130Constants::SCOPE_OPERATOR),
                Address::ZERO,
                B256::ZERO,
                false,
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
            AccountConfigurationEvents::pack_actor_data(&config, MANAGER, COMMITMENT, true)
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
            AccountChangeApplier::apply_create(acc, &entry, MAX_CODE).unwrap();
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
