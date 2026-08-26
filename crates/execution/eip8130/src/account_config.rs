//! Native read-only mirror of the EIP-8130 `AccountConfiguration` system
//! contract's storage layout and its storage-view functions.

use alloy_primitives::{Address, B256, U256};
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, Result, StorageKey};

/// Read-only view over the EIP-8130 `AccountConfiguration` system contract's
/// storage, mirroring its layout (plain sequential slots, no ERC-7201
/// namespace):
///
/// ```solidity
/// mapping(bytes32 actorId => mapping(address account => ActorConfig)) _actorConfig;     // slot 0
/// mapping(bytes32 actorId => mapping(address account => bytes32))     _policyCommitment; // slot 1
/// mapping(bytes32 actorId => mapping(address account => address))     _policyManager;    // slot 2
/// mapping(address account => AccountState)                            _accountState;     // slot 3
/// ```
///
/// `account` is the inner mapping key (matching the contract's ERC-7562
/// storage-access requirement). The packed `ActorConfig` and `AccountState`
/// slots are modelled as a raw [`U256`] and unpacked by [`ActorConfig::from_word`]
/// / [`AccountState::from_word`].
#[contract(addr = Self::ADDRESS)]
pub struct AccountConfigurationStorage {
    /// slot 0: per-actor configuration (packed `ActorConfig` word).
    pub actor_config: Mapping<B256, Mapping<Address, U256>>,
    /// slot 1: per-actor signed policy commitment (set for `SCOPE_POLICY` actors).
    pub policy_commitment: Mapping<B256, Mapping<Address, B256>>,
    /// slot 2: per-actor policy manager (set for `SCOPE_POLICY` actors).
    pub policy_manager: Mapping<B256, Mapping<Address, Address>>,
    /// slot 3: per-account state (packed `AccountState` word).
    pub account_state: Mapping<Address, U256>,
}

impl AccountConfigurationStorage<'_> {
    /// Account Configuration system-contract address.
    ///
    /// Pinned to [`Eip8130Contracts::ACCOUNT_CONFIG`]; provisional and tracks the
    /// reference contract's bytecode (see the crate docs).
    pub const ADDRESS: Address = Eip8130Contracts::ACCOUNT_CONFIG;

    /// Base storage slot of the per-account state mapping.
    pub const ACCOUNT_STATE_BASE_SLOT: U256 = slots::ACCOUNT_STATE;

    /// Returns the storage slot that holds the state for `account`.
    pub fn account_state_slot(account: Address) -> B256 {
        B256::from(account.mapping_slot(Self::ACCOUNT_STATE_BASE_SLOT).to_be_bytes::<32>())
    }

    /// Reads the raw `actor_config[actor_id][account]` storage slot verbatim,
    /// with **no** inline-self blend. An absent entry reads back as an all-zero
    /// word, i.e. [`ActorConfig::EMPTY`].
    ///
    /// This is the contract's internal `_actorConfig[actorId][account]` read, not
    /// its public `getActorConfig` view — the live inline k1 self has no entry
    /// here (its config lives in [`AccountState`]). For the effective config that
    /// blends the inline self (the `getActorConfig` mirror), use
    /// [`Self::resolve_actor_config`].
    pub fn actor_config_slot(&self, account: Address, actor_id: B256) -> Result<ActorConfig> {
        Ok(ActorConfig::from_word(self.actor_config.at(&actor_id).at(&account).read()?))
    }

    /// Resolves the *effective* [`ActorConfig`] for `(account, actor_id)`,
    /// mirroring the contract's `getActorConfig`: an explicit `actor_config`
    /// entry wins; otherwise, for the self-actorId, the live inline secp256k1
    /// self is returned as a synthesized k1 config
    /// (`{K1_AUTHENTICATOR, default_eoa_scope, default_eoa_expiry}`); a revoked
    /// self or an unknown actor resolves to [`ActorConfig::EMPTY`].
    ///
    /// This is the single home for the "explicit entry vs inline k1 self" branch:
    /// a live inline self is modelled as what it is — a k1 actor whose bytes live
    /// inline rather than in an `actor_config` slot — so downstream readers
    /// ([`Self::is_actor`], [`Self::get_policy`]) need no self-key special-casing.
    /// `DEFAULT_EOA_REVOKED` gates only the inline self; an explicit non-k1 self
    /// entry is unaffected and wins here regardless of the flag.
    pub fn resolve_actor_config(&self, account: Address, actor_id: B256) -> Result<ActorConfig> {
        let stored = self.actor_config_slot(account, actor_id)?;
        if !stored.is_empty() {
            return Ok(stored);
        }
        if actor_id == Self::self_actor_id(account) {
            let state = self.get_account_state(account)?;
            if !state.default_eoa_revoked() {
                return Ok(ActorConfig {
                    authenticator: Eip8130Constants::K1_AUTHENTICATOR,
                    scope: state.default_eoa_scope,
                    expiry: state.default_eoa_expiry,
                });
            }
        }
        Ok(ActorConfig::EMPTY)
    }

    /// Resolves the *live* effective [`ActorConfig`] for `(account, actor_id)`,
    /// mirroring `Keystore._resolveActorConfig`: expired actors read as
    /// [`ActorConfig::EMPTY`], so an expired slot is treated as a new add.
    pub fn resolve_live_actor_config(
        &self,
        account: Address,
        actor_id: B256,
        now: u64,
    ) -> Result<ActorConfig> {
        let config = self.resolve_actor_config(account, actor_id)?;
        if config.is_empty() {
            return Ok(ActorConfig::EMPTY);
        }
        if config.expiry != 0 && now > config.expiry {
            return Ok(ActorConfig::EMPTY);
        }
        Ok(config)
    }

    /// Mirrors `AccountConfiguration.isActor`: `true` for any live actor — an
    /// explicit `actor_config` entry, or the inline secp256k1 self while its
    /// `DEFAULT_EOA_REVOKED` flag is unset. Both cases collapse to "the resolved
    /// effective config is non-empty".
    ///
    /// Like the contract, this does **not** check expiry: an expired-but-not-
    /// revoked actor still reports as an actor (the revoke path relies on it).
    pub fn is_actor(&self, account: Address, actor_id: B256) -> Result<bool> {
        Ok(!self.resolve_actor_config(account, actor_id)?.is_empty())
    }

    /// Resolves an actor's effective policy gate target and signed commitment.
    /// An ungated actor resolves to `(address(0), bytes32(0))`; a gated one to
    /// `(manager, commitment)`.
    ///
    /// Unlike the contract's `getPolicy` — a pure two-slot raw read — this gates
    /// on the actor's live scope (via [`Self::resolve_actor_config`]): an ungated
    /// actor never has its policy slots written, so it resolves to `(0, 0)`. A
    /// gated actor returns the raw slots, which may *themselves* be zero — a gated
    /// actor may carry a zero `manager` and/or `commitment` (`slice_policy` writes
    /// policy data verbatim); a zero `manager` simply gates the key to
    /// `address(0)` (no productive target). So a `(0, 0)` result does not by
    /// itself distinguish "ungated" from "gated with empty policy data". This is a
    /// resolver, not a 1:1 mirror; for the raw reads use
    /// [`Self::get_policy_manager`] / [`Self::get_policy_commitment`].
    pub fn get_policy(&self, account: Address, actor_id: B256) -> Result<(Address, B256)> {
        let scope = self.resolve_actor_config(account, actor_id)?.scope;
        if scope & Eip8130Constants::SCOPE_POLICY == 0 {
            return Ok((Address::ZERO, B256::ZERO));
        }
        let manager = self.policy_manager.at(&actor_id).at(&account).read()?;
        let commitment = self.policy_commitment.at(&actor_id).at(&account).read()?;
        Ok((manager, commitment))
    }

    /// Reads only the stored policy *manager* slot for `(account, actor_id)`,
    /// without the `actor_config` re-read that [`Self::get_policy`] performs to
    /// gate on `SCOPE_POLICY`. Callers that already hold the [`ActorConfig`] (and
    /// have confirmed that bit) use this to resolve a policy target with
    /// a single trie/DB hit on the validation hot path. Mirrors the manager read
    /// in `AccountConfiguration._resolvePolicyTarget`.
    pub fn get_policy_manager(&self, account: Address, actor_id: B256) -> Result<Address> {
        self.policy_manager.at(&actor_id).at(&account).read()
    }

    /// Reads only the stored policy *commitment* slot for `(account, actor_id)`,
    /// the single-SLOAD read a policy manager performs to validate a dispatched
    /// 8130 transaction against the actor's signed commitment. This is a raw slot
    /// read: it is written (verbatim) only while the actor has `SCOPE_POLICY`, but
    /// a gated actor may legitimately store a zero commitment (`slice_policy`
    /// treats it as a valid "no parameters" value), so a zero return does **not**
    /// by itself imply "no policy / no actor" — pair it with the actor's scope for
    /// that distinction. Mirrors `AccountConfiguration.getPolicyCommitment`.
    pub fn get_policy_commitment(&self, account: Address, actor_id: B256) -> Result<B256> {
        self.policy_commitment.at(&actor_id).at(&account).read()
    }

    /// Returns the per-account [`AccountState`] (sequences + lock fields).
    pub fn get_account_state(&self, account: Address) -> Result<AccountState> {
        Ok(AccountState::from_word(self.account_state.at(&account).read()?))
    }

    /// Mirrors `AccountConfiguration.getChangeSequences`: `(multichain, local)`.
    pub fn get_change_sequences(&self, account: Address) -> Result<(u64, u64)> {
        let state = self.get_account_state(account)?;
        Ok((state.multichain_sequence, state.local_sequence))
    }

    /// Mirrors `AccountConfiguration._isInitialized`: `true` once the account has
    /// any EIP-8130 state. `local_sequence > 0` is set at bootstrap
    /// (created/imported) and doubles as the initialized flag, but it is not the
    /// only channel: a never-bootstrapped account can establish state through a
    /// `chain_id == 0` (multichain) `applySignedActorChanges` — authenticated by
    /// its still-live implicit default EOA — which bumps `multichain_sequence`
    /// while leaving `local_sequence` at 0. The contract treats that account as
    /// initialized (blocking a later create/import from clobbering the
    /// multichain-established state), so the local word (`local_sequence` or
    /// `local_epoch`) and `multichain_sequence` must all be checked.
    pub fn is_initialized(&self, account: Address) -> Result<bool> {
        Ok(self.get_account_state(account)?.is_initialized())
    }

    /// Mirrors `AccountConfiguration._isLocked`: not locked unless `FLAG_LOCKED`
    /// is set; hard-locked (frozen) while `FLAG_UNLOCK_INITIATED` is clear; once
    /// an unlock is initiated, frozen only until `now >= lock_union` (`unlocks_at`).
    /// `now` is supplied by the caller (block timestamp at inclusion, wall-clock
    /// in the pool), since the reader has no block context.
    pub fn is_locked(&self, account: Address, now: u64) -> Result<bool> {
        Ok(self.get_account_state(account)?.is_locked(now))
    }

    /// Mirrors `AccountConfiguration.isContractEstablished`: `true` once the
    /// account has been keystore-established (create/import). Used to gate code
    /// delegation onto an empty-code account (see [`crate::DelegationEffect`]).
    pub fn is_contract_established(&self, account: Address) -> Result<bool> {
        Ok(self.get_account_state(account)?.contract_established())
    }

    /// Mirrors `AccountConfiguration.getLockStatus`.
    pub fn get_lock_status(&self, account: Address, now: u64) -> Result<LockStatus> {
        Ok(self.get_account_state(account)?.lock_status(now))
    }

    /// The implicit-EOA self-actor id for `account`:
    /// `bytes32(uint256(uint160(account)))`, i.e. the address right-aligned in the
    /// low 20 bytes (matches the finalized `Keystore.ActorId.fromAddress`).
    #[must_use]
    pub fn self_actor_id(account: Address) -> B256 {
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(account.as_slice());
        B256::from(word)
    }

    /// Writes `config` to the `(account, actor_id)` `actor_config` slot. Writing
    /// [`ActorConfig::EMPTY`] zeroes the slot, mirroring Solidity `delete`.
    pub fn set_actor_config(
        &mut self,
        account: Address,
        actor_id: B256,
        config: ActorConfig,
    ) -> Result<()> {
        self.actor_config.at_mut(&actor_id).at_mut(&account).write(config.to_word())
    }

    /// Clears the `(account, actor_id)` `actor_config` slot (Solidity `delete`).
    pub fn clear_actor_config(&mut self, account: Address, actor_id: B256) -> Result<()> {
        self.set_actor_config(account, actor_id, ActorConfig::EMPTY)
    }

    /// Writes the packed [`AccountState`] word for `account`.
    pub fn set_account_state(&mut self, account: Address, state: AccountState) -> Result<()> {
        self.account_state.at_mut(&account).write(state.to_word())
    }

    /// Writes the `(account, actor_id)` policy slots. A zero `manager` /
    /// `commitment` zeroes its slot, so passing both zero mirrors the Solidity
    /// `delete` of an actor's policy on revoke.
    pub fn set_policy(
        &mut self,
        account: Address,
        actor_id: B256,
        manager: Address,
        commitment: B256,
    ) -> Result<()> {
        self.policy_manager.at_mut(&actor_id).at_mut(&account).write(manager)?;
        self.policy_commitment.at_mut(&actor_id).at_mut(&account).write(commitment)
    }

    /// Clears both policy slots for `(account, actor_id)` (Solidity `delete`).
    pub fn clear_policy(&mut self, account: Address, actor_id: B256) -> Result<()> {
        self.set_policy(account, actor_id, Address::ZERO, B256::ZERO)
    }
}

/// Decoded `Keystore.ActorConfig` (one packed storage slot).
///
/// Solidity layout `{address authenticator; uint48 expiry; uint16 scope;}` packs
/// right-aligned in declaration order, lowest-order field first, into a single
/// 32-byte slot, with the top 4 bytes reserved padding that MUST stay zero:
///
/// ```text
/// bytes (big-endian):  [0..4) reserved | [4..6) scope | [6..12) expiry | [12..32) authenticator
/// bits  (LSB-first):   authenticator 0..160 | expiry 160..208 | scope 208..224 | reserved 224..256
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActorConfig {
    /// Authenticator address bound to the actor (`address(0)` = empty slot,
    /// `address(1)` = native k1/ecrecover, any other = `IAuthenticator` contract).
    pub authenticator: Address,
    /// Unix-seconds expiry; `0 = no expiry`. The actor is invalid once
    /// `block.timestamp > expiry`.
    pub expiry: u64,
    /// Elevated-scope bitfield (`uint16`; `0 = unrestricted`).
    pub scope: u16,
}

impl ActorConfig {
    /// The empty (unset) actor config: a zeroed storage slot.
    pub const EMPTY: Self = Self { authenticator: Address::ZERO, expiry: 0, scope: 0 };

    /// Returns whether the reserved high 32 bits of a packed word are non-zero.
    #[must_use]
    pub fn has_nonzero_reserved(word: U256) -> bool {
        word.to_be_bytes::<32>()[..4].iter().any(|&byte| byte != 0)
    }

    /// Unpacks a raw `ActorConfig` storage word.
    #[must_use]
    pub fn from_word(word: U256) -> Self {
        let b = word.to_be_bytes::<32>();
        let mut expiry = [0u8; 8];
        expiry[2..].copy_from_slice(&b[6..12]); // uint48: 6 bytes, big-endian
        Self {
            authenticator: Address::from_slice(&b[12..32]),
            expiry: u64::from_be_bytes(expiry),
            scope: u16::from_be_bytes([b[4], b[5]]), // uint16: 2 bytes
        }
    }

    /// `true` if the slot is empty (no authenticator bound).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.authenticator == Address::ZERO
    }

    /// Packs this config into its raw storage word — the exact inverse of
    /// [`Self::from_word`].
    ///
    /// `expiry` must fit in `uint48` (the storage field width); higher bytes are
    /// dropped. Values sourced from [`Self::from_word`] or ABI decoding always
    /// satisfy this, so the `debug_assert!` only guards hand-constructed misuse.
    #[must_use]
    pub fn to_word(&self) -> U256 {
        debug_assert!(self.expiry >> 48 == 0, "expiry exceeds uint48 storage width");
        let mut b = [0u8; 32];
        b[12..32].copy_from_slice(self.authenticator.as_slice());
        b[6..12].copy_from_slice(&self.expiry.to_be_bytes()[2..]); // uint48: low 6 bytes
        b[4..6].copy_from_slice(&self.scope.to_be_bytes()); // uint16: 2 bytes
        U256::from_be_bytes(b)
    }
}

/// Decoded `Keystore.AccountState` (one packed storage slot).
///
/// Solidity layout `{uint64 multichainSequence; uint32 localSequence; uint32
/// localEpoch; uint8 flags; uint48 lockUnion; uint48 defaultEOAExpiry; uint16
/// defaultEOAScope;}`, packed right-aligned, lowest-order field first; the top
/// byte of the slot is reserved padding that MUST stay zero:
///
/// ```text
/// bits (LSB-first): multichain 0..64 | localSequence 64..96 | localEpoch 96..128 | flags 128..136 | lock_union 136..184 | defaultEOAExpiry 184..232 | defaultEOAScope 232..248 | reserved 248..256
/// ```
///
/// The local replay counter is split into two adjacent `uint32` fields —
/// `local_sequence` (low) and `local_epoch` (high) — which occupy the same 8
/// bytes as, and read identically to, the single `localEpoch(32) ||
/// localSequence(32)` word committed in a signed batch's `sequence`.
///
/// `lock_union` is a `uint48` union field (see [Account Lock] in the spec): while
/// [`Eip8130Constants::FLAG_UNLOCK_INITIATED`] is clear it holds the configured
/// `unlock_delay` (seconds, `uint16` range); while set it holds `unlocks_at` (the
/// timestamp at which a pending unlock takes effect). Lock state is mutated only
/// through the EVM signed-change entry point; the native path only reads it (see
/// [`Self::is_locked`]).
///
/// The `default_eoa_*` fields are the inline home for the account's own
/// secp256k1 ("self") key: when `DEFAULT_EOA_REVOKED` is unset, a k1 signature
/// recovering to the account authenticates with this inline config — all-zero
/// is the implicit full owner, a non-zero scope/expiry is a scoped self — so the
/// entire self key resolves in a single account-state SLOAD. The
/// `actor_config(self)` slot is reserved for a *non*-k1 self authenticator
/// (e.g. a post-quantum verifier returning the self-actorId); the inline k1
/// self and a non-k1 self are mutually exclusive.
///
/// [Account Lock]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountState {
    /// Sequence for the multichain (`chain_id == 0`) signed-change channel. A
    /// non-zero value also marks the account initialized (see [`Self`] /
    /// `is_initialized`).
    pub multichain_sequence: u64,
    /// Local-channel sequence (`uint32`). Set to 1 at bootstrap (create/import);
    /// a non-zero value also marks the account initialized. Reset to 0 by an
    /// `IncrementLocalEpoch` op (which bumps `local_epoch`).
    pub local_sequence: u64,
    /// Local-channel epoch (`uint32`). Incremented by `IncrementLocalEpoch`,
    /// invalidating every unlanded local signature at a prior epoch. A non-zero
    /// value also marks the account initialized.
    pub local_epoch: u64,
    /// Account flags bitfield: bit 0
    /// ([`Eip8130Constants::FLAG_CONTRACT_ESTABLISHED`]) marks a
    /// keystore-established account (create/import) that must not be treated as a
    /// proven-key EOA even when its code is empty; bit 1
    /// ([`Eip8130Constants::DEFAULT_EOA_REVOKED`]) disables the inline secp256k1
    /// self key; bit 2 ([`Eip8130Constants::FLAG_LOCKED`]) freezes actor
    /// configuration; bit 3 ([`Eip8130Constants::FLAG_UNLOCK_INITIATED`]) selects
    /// the `lock_union` interpretation.
    pub flags: u8,
    /// `uint48` lock union: `unlock_delay` (seconds) while `FLAG_UNLOCK_INITIATED`
    /// is clear, else `unlocks_at` (Unix-seconds timestamp).
    pub lock_union: u64,
    /// Inline self-key Unix-seconds expiry (`0` = no expiry). The self key is
    /// invalid once `now > default_eoa_expiry`.
    pub default_eoa_expiry: u64,
    /// Inline self-key scope bitfield (`uint16`; `0` = unrestricted full owner).
    /// Governs only when the self key is live (`!default_eoa_revoked()`).
    pub default_eoa_scope: u16,
}

impl AccountState {
    /// `type(uint48).max` — the `unlocks_at` value `getLockStatus` synthesizes for
    /// a hard-locked account (`FLAG_LOCKED` set, `FLAG_UNLOCK_INITIATED` clear),
    /// where `lock_union` actually stores the configured delay rather than a
    /// timestamp. Not a stored sentinel.
    pub const UNLOCKS_AT_MAX: u64 = (1 << 48) - 1;

    /// Unpacks a raw `AccountState` storage word.
    #[must_use]
    pub fn from_word(word: U256) -> Self {
        let b = word.to_be_bytes::<32>();
        let mut multichain = [0u8; 8];
        let mut local_sequence = [0u8; 8];
        let mut local_epoch = [0u8; 8];
        let mut lock_union = [0u8; 8];
        let mut default_eoa_expiry = [0u8; 8];
        multichain.copy_from_slice(&b[24..32]); // uint64 at bits 0..64
        local_sequence[4..].copy_from_slice(&b[20..24]); // uint32 at bits 64..96
        local_epoch[4..].copy_from_slice(&b[16..20]); // uint32 at bits 96..128
        lock_union[2..].copy_from_slice(&b[9..15]); // uint48 at bits 136..184
        default_eoa_expiry[2..].copy_from_slice(&b[3..9]); // uint48 at bits 184..232
        Self {
            multichain_sequence: u64::from_be_bytes(multichain),
            local_sequence: u64::from_be_bytes(local_sequence),
            local_epoch: u64::from_be_bytes(local_epoch),
            flags: b[15], // uint8 at bits 128..136
            lock_union: u64::from_be_bytes(lock_union),
            default_eoa_expiry: u64::from_be_bytes(default_eoa_expiry),
            default_eoa_scope: u16::from_be_bytes([b[1], b[2]]), // uint16 at bits 232..248
        }
    }

    /// `true` when the implicit default-EOA path is disabled for this account
    /// (the `DEFAULT_EOA_REVOKED` flag bit is set).
    #[must_use]
    pub const fn default_eoa_revoked(&self) -> bool {
        self.flags & Eip8130Constants::DEFAULT_EOA_REVOKED != 0
    }

    /// `true` when this account was keystore-established (create/import) and so
    /// must not be treated as a proven-key EOA — even with empty code. Mirrors
    /// `Keystore.isContractEstablished` (the `FLAG_CONTRACT_ESTABLISHED` bit).
    #[must_use]
    pub const fn contract_established(&self) -> bool {
        self.flags & Eip8130Constants::FLAG_CONTRACT_ESTABLISHED != 0
    }

    /// Mirrors `AccountConfiguration._isLocked`: revoke and lock-delay changes are
    /// frozen unless an initiated unlock's timestamp has elapsed; `AuthorizeActor`
    /// may still add a new actor or re-lease a live one (expiry only) when the
    /// granted expiry outlives the unlock floor.
    #[must_use]
    pub const fn is_locked(&self, now: u64) -> bool {
        if self.flags & Eip8130Constants::FLAG_LOCKED == 0 {
            return false; // not locked
        }
        if self.flags & Eip8130Constants::FLAG_UNLOCK_INITIATED == 0 {
            return true; // hard-locked, no pending unlock
        }
        now < self.lock_union // pending unlock: frozen until the timestamp elapses
    }

    /// The soonest timestamp the account can be unlocked. Mirrors
    /// `Keystore._unlockFloor`. Only meaningful while [`Self::is_locked`] is
    /// `true`: hard-locked accounts use `now + delay`; pending unlock uses
    /// `unlocks_at` (`lock_union`).
    #[must_use]
    pub const fn unlock_floor(&self, now: u64) -> u64 {
        if self.flags & Eip8130Constants::FLAG_UNLOCK_INITIATED != 0 {
            self.lock_union
        } else {
            now.saturating_add(self.lock_union & 0xFFFF)
        }
    }

    /// Mirrors `AccountConfiguration.getLockStatus`, deriving the human-readable
    /// lock view from the packed `flags`/`lock_union` union.
    #[must_use]
    pub const fn lock_status(&self, now: u64) -> LockStatus {
        if self.flags & Eip8130Constants::FLAG_LOCKED == 0 {
            return LockStatus {
                locked: false,
                has_initiated_unlock: false,
                unlocks_at: 0,
                unlock_delay: 0,
            };
        }
        if self.flags & Eip8130Constants::FLAG_UNLOCK_INITIATED == 0 {
            // Hard-locked: lock_union holds the configured delay; synthesize the
            // max sentinel for `unlocks_at`. The delay is stored in `uint16` range
            // by the lock op; mask explicitly to make the truncation intentional
            // and mirror the contract's `uint16(config.lockUnion)` cast.
            return LockStatus {
                locked: true,
                has_initiated_unlock: false,
                unlocks_at: Self::UNLOCKS_AT_MAX,
                unlock_delay: (self.lock_union & 0xFFFF) as u16,
            };
        }
        // Unlock initiated: lock_union holds the effective unlock timestamp.
        LockStatus {
            locked: now < self.lock_union,
            has_initiated_unlock: true,
            unlocks_at: self.lock_union,
            unlock_delay: 0,
        }
    }

    /// Mirrors `AccountConfiguration._isInitialized`: `true` once the account has
    /// any EIP-8130 state on either channel — a non-zero local word
    /// (`local_sequence` or `local_epoch`) or a non-zero `multichain_sequence`.
    /// The single source of truth for the initialized predicate so callers
    /// (e.g. the create guard in [`AccountChangeApplier::apply_create`]) cannot
    /// drift from it.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.local_sequence > 0 || self.local_epoch > 0 || self.multichain_sequence > 0
    }

    /// Packs this state into its raw storage word — the exact inverse of
    /// [`Self::from_word`].
    ///
    /// `local_sequence`/`local_epoch` must fit in `uint32`, `lock_union` and
    /// `default_eoa_expiry` in `uint48` (their storage field widths); higher
    /// bytes are dropped. Values sourced from [`Self::from_word`] or ABI decoding
    /// always satisfy this, so the `debug_assert!`s only guard hand-constructed
    /// misuse.
    #[must_use]
    pub fn to_word(&self) -> U256 {
        debug_assert!(
            self.local_sequence >> 32 == 0,
            "local_sequence exceeds uint32 storage width"
        );
        debug_assert!(self.local_epoch >> 32 == 0, "local_epoch exceeds uint32 storage width");
        debug_assert!(self.lock_union >> 48 == 0, "lock_union exceeds uint48 storage width");
        debug_assert!(
            self.default_eoa_expiry >> 48 == 0,
            "default_eoa_expiry exceeds uint48 storage width"
        );
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&self.multichain_sequence.to_be_bytes());
        b[20..24].copy_from_slice(&self.local_sequence.to_be_bytes()[4..]); // uint32: low 4 bytes
        b[16..20].copy_from_slice(&self.local_epoch.to_be_bytes()[4..]); // uint32: low 4 bytes
        b[15] = self.flags;
        b[9..15].copy_from_slice(&self.lock_union.to_be_bytes()[2..]); // uint48: low 6 bytes
        b[3..9].copy_from_slice(&self.default_eoa_expiry.to_be_bytes()[2..]); // uint48: low 6 bytes
        b[1..3].copy_from_slice(&self.default_eoa_scope.to_be_bytes()); // uint16: 2 bytes
        U256::from_be_bytes(b)
    }
}

/// Decoded result of `AccountConfiguration.getLockStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LockStatus {
    /// `true` while the account is frozen at the queried timestamp.
    pub locked: bool,
    /// `true` once the `applySignedLockChanges` unlock op (`UNLOCK_OP`) has run
    /// (`FLAG_UNLOCK_INITIATED` set), i.e. an unlock is pending.
    pub has_initiated_unlock: bool,
    /// The effective unlock timestamp: the stored `unlocks_at` once an unlock is
    /// initiated, or [`AccountState::UNLOCKS_AT_MAX`] synthesized while
    /// hard-locked.
    pub unlocks_at: u64,
    /// The configured unlock delay in seconds (reported only while hard-locked).
    pub unlock_delay: u16,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, address, b256};
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};

    use super::*;

    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000a1");
    const ACTOR: B256 = b256!("0x00000000000000000000000000000000000000b2000000000000000000000000");

    /// Canonical Solidity packing of `ActorConfig` (each field at its bit
    /// offset). Independent of the byte-slice [`ActorConfig::from_word`] decoder,
    /// so agreement cross-checks the layout.
    fn pack_actor_config(authenticator: Address, scope: u16, expiry: u64) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(expiry) << 160)
            | (U256::from(scope) << 208)
    }

    fn pack_account_state(
        multichain: u64,
        local: u64,
        flags: u8,
        lock_union: u64,
        default_eoa_scope: u16,
        default_eoa_expiry: u64,
    ) -> U256 {
        U256::from(multichain)
            | (U256::from(local) << 64)
            | (U256::from(flags) << 128)
            | (U256::from(lock_union) << 136)
            | (U256::from(default_eoa_expiry) << 184)
            | (U256::from(default_eoa_scope) << 232)
    }

    /// Packs an `AccountState` word with an explicit local epoch (high 32 bits of
    /// the local word), for tests that exercise the epoch split.
    fn pack_account_state_epoch(
        multichain: u64,
        local_sequence: u64,
        local_epoch: u64,
        flags: u8,
        lock_union: u64,
        default_eoa_scope: u16,
        default_eoa_expiry: u64,
    ) -> U256 {
        U256::from(multichain)
            | (U256::from(local_sequence) << 64)
            | (U256::from(local_epoch) << 96)
            | (U256::from(flags) << 128)
            | (U256::from(lock_union) << 136)
            | (U256::from(default_eoa_expiry) << 184)
            | (U256::from(default_eoa_scope) << 232)
    }

    #[test]
    fn account_state_slot_matches_generated_storage_layout() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let account_config = AccountConfigurationStorage::new(ctx);
            let generated = account_config.account_state.at(&ACCOUNT).slot();
            assert_eq!(
                AccountConfigurationStorage::account_state_slot(ACCOUNT),
                B256::from(generated.to_be_bytes::<32>())
            );
        });
    }

    #[test]
    fn actor_config_unpacks_each_field_from_its_slot_position() {
        let authenticator = address!("0x1234567890abcDEF1234567890aBcdef12345678");
        let expiry = (1u64 << 48) - 1; // full uint48
        let word = pack_actor_config(authenticator, 0xAB, expiry);

        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(word).unwrap();
            let config = acc.actor_config_slot(ACCOUNT, ACTOR).unwrap();
            assert_eq!(config.authenticator, authenticator);
            assert_eq!(config.scope, 0xAB);
            assert_eq!(config.expiry, expiry);
            assert!(!config.is_empty());
        });
    }

    #[test]
    fn absent_actor_reads_back_empty() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let config = AccountConfigurationStorage::new(ctx).actor_config_slot(ACCOUNT, ACTOR);
            assert_eq!(config.unwrap(), ActorConfig::EMPTY);
        });
    }

    #[test]
    fn resolve_actor_config_blends_inline_self() {
        let bound = address!("0x00000000000000000000000000000000000000ff");
        let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);

            // Unknown non-self actor, empty slot -> empty.
            assert_eq!(acc.resolve_actor_config(ACCOUNT, ACTOR).unwrap(), ActorConfig::EMPTY);

            // Explicit entry wins verbatim.
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(pack(bound)).unwrap();
            assert_eq!(
                acc.resolve_actor_config(ACCOUNT, ACTOR).unwrap(),
                ActorConfig { authenticator: bound, scope: 0, expiry: 0 }
            );

            // Live inline self (no explicit entry) -> synthesized k1 config.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, 0, 0, Eip8130Constants::SCOPE_SENDER, 42))
                .unwrap();
            assert_eq!(
                acc.resolve_actor_config(ACCOUNT, self_id).unwrap(),
                ActorConfig {
                    authenticator: Eip8130Constants::K1_AUTHENTICATOR,
                    scope: Eip8130Constants::SCOPE_SENDER,
                    expiry: 42,
                }
            );

            // Revoked inline self -> empty.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, Eip8130Constants::DEFAULT_EOA_REVOKED, 0, 0, 0))
                .unwrap();
            assert_eq!(acc.resolve_actor_config(ACCOUNT, self_id).unwrap(), ActorConfig::EMPTY);

            // Explicit (non-k1) self entry wins even while the revoked flag is set.
            acc.actor_config.at_mut(&self_id).at_mut(&ACCOUNT).write(pack(bound)).unwrap();
            assert_eq!(
                acc.resolve_actor_config(ACCOUNT, self_id).unwrap(),
                ActorConfig { authenticator: bound, scope: 0, expiry: 0 }
            );
        });
    }

    #[test]
    fn is_actor_matches_contract_predicate() {
        let mut storage = HashMapStorageProvider::new(1);
        let bound = address!("0x00000000000000000000000000000000000000ff");
        let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);

            // Bound to a real authenticator -> actor.
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(pack(bound)).unwrap();
            assert!(acc.is_actor(ACCOUNT, ACTOR).unwrap());

            // Empty slot, non-self actor id -> not an actor.
            let other = b256!("0x00000000000000000000000000000000000000cc000000000000000000000000");
            assert!(!acc.is_actor(ACCOUNT, other).unwrap());

            // Empty slot, self actor id, flag unset -> implicit default EOA actor.
            assert!(acc.is_actor(ACCOUNT, self_id).unwrap());

            // Empty slot, self actor id, DEFAULT_EOA_REVOKED set -> not an actor.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, Eip8130Constants::DEFAULT_EOA_REVOKED, 0, 0, 0))
                .unwrap();
            assert!(!acc.is_actor(ACCOUNT, self_id).unwrap());

            // Explicit self entry stays live even with the flag set (re-registered
            // scoped/owner k1 self key); the entry-exists => flag-set invariant.
            acc.actor_config.at_mut(&self_id).at_mut(&ACCOUNT).write(pack(bound)).unwrap();
            assert!(acc.is_actor(ACCOUNT, self_id).unwrap());
        });
    }

    #[test]
    fn get_policy_resolves_only_when_gated() {
        let manager = address!("0x00000000000000000000000000000000000000d4");
        let commitment =
            b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);

            // Ungated actor -> zeroed regardless of stored slots.
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(pack(manager)).unwrap();
            acc.policy_manager.at_mut(&ACTOR).at_mut(&ACCOUNT).write(manager).unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, ACTOR).unwrap(), (Address::ZERO, B256::ZERO));

            // Gated actor -> (manager, commitment).
            let gated = pack_actor_config(manager, Eip8130Constants::SCOPE_POLICY, 0);
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(gated).unwrap();
            acc.policy_commitment.at_mut(&ACTOR).at_mut(&ACCOUNT).write(commitment).unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, ACTOR).unwrap(), (manager, commitment));
        });
    }

    #[test]
    fn get_policy_resolves_inline_self_key() {
        let manager = address!("0x00000000000000000000000000000000000000d4");
        let commitment =
            b256!("0x2222222222222222222222222222222222222222222222222222222222222222");
        let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);
            acc.policy_manager.at_mut(&self_id).at_mut(&ACCOUNT).write(manager).unwrap();
            acc.policy_commitment.at_mut(&self_id).at_mut(&ACCOUNT).write(commitment).unwrap();

            // Live full-owner self -> ungated, slots ignored.
            assert_eq!(acc.get_policy(ACCOUNT, self_id).unwrap(), (Address::ZERO, B256::ZERO));

            // Live scoped self with an inline gate -> (manager, commitment).
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, 0, 0, Eip8130Constants::SCOPE_POLICY, 0))
                .unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, self_id).unwrap(), (manager, commitment));

            // Revoked self -> ungated regardless of the inline scope.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, Eip8130Constants::DEFAULT_EOA_REVOKED, 0, 0, 0))
                .unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, self_id).unwrap(), (Address::ZERO, B256::ZERO));
        });
    }

    #[test]
    fn get_policy_resolves_non_k1_self_while_default_eoa_revoked() {
        // A non-k1 self authenticator homed at the self-actorId coexists with
        // DEFAULT_EOA_REVOKED (authorizing it *sets* that flag via mutual
        // exclusion). The flag disables only the inline k1 self, so an explicit
        // entry's policy must still resolve — the explicit-entry branch wins.
        let authenticator = address!("0x00000000000000000000000000000000000000e5");
        let manager = address!("0x00000000000000000000000000000000000000d4");
        let commitment =
            b256!("0x3333333333333333333333333333333333333333333333333333333333333333");
        let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);
            let gated = pack_actor_config(authenticator, Eip8130Constants::SCOPE_POLICY, 0);
            acc.actor_config.at_mut(&self_id).at_mut(&ACCOUNT).write(gated).unwrap();
            acc.policy_manager.at_mut(&self_id).at_mut(&ACCOUNT).write(manager).unwrap();
            acc.policy_commitment.at_mut(&self_id).at_mut(&ACCOUNT).write(commitment).unwrap();
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, Eip8130Constants::DEFAULT_EOA_REVOKED, 0, 0, 0))
                .unwrap();

            assert_eq!(acc.get_policy(ACCOUNT, self_id).unwrap(), (manager, commitment));
        });
    }

    #[test]
    fn get_policy_manager_reads_only_the_manager_slot() {
        let manager = address!("0x00000000000000000000000000000000000000d4");
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);
            // No actor_config written: the targeted read does not gate on it.
            acc.policy_manager.at_mut(&ACTOR).at_mut(&ACCOUNT).write(manager).unwrap();
            assert_eq!(acc.get_policy_manager(ACCOUNT, ACTOR).unwrap(), manager);
        });
    }

    #[test]
    fn account_state_unpacks_sequences_and_lock_fields() {
        let expiry = (1u64 << 48) - 1; // full uint48
        let lock_union = (1u64 << 48) - 1; // full uint48
        let word = pack_account_state(
            7,
            3,
            Eip8130Constants::DEFAULT_EOA_REVOKED | Eip8130Constants::FLAG_LOCKED,
            lock_union,
            0xAB,
            expiry,
        );
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);
            acc.account_state.at_mut(&ACCOUNT).write(word).unwrap();

            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert_eq!(state.multichain_sequence, 7);
            assert_eq!(state.local_sequence, 3);
            assert_eq!(state.lock_union, lock_union);
            assert!(state.default_eoa_revoked());
            assert_eq!(state.default_eoa_scope, 0xAB);
            assert_eq!(state.default_eoa_expiry, expiry);
            assert_eq!(acc.get_change_sequences(ACCOUNT).unwrap(), (7, 3));
            assert!(acc.is_initialized(ACCOUNT).unwrap());
        });
    }

    #[test]
    fn is_initialized_covers_both_sequence_channels() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);

            // Fresh account: neither channel set -> uninitialized.
            assert!(!acc.is_initialized(ACCOUNT).unwrap());

            // Multichain-only: a chain_id == 0 change bumped `multichain_sequence`
            // on a never-bootstrapped account, `local_sequence` still 0. The
            // contract's `_isInitialized` (local || multichain) treats this as
            // initialized, so the native mirror must too.
            acc.account_state.at_mut(&ACCOUNT).write(pack_account_state(1, 0, 0, 0, 0, 0)).unwrap();
            assert!(acc.is_initialized(ACCOUNT).unwrap());

            // Local-only: the bootstrap (create/import) channel.
            acc.account_state.at_mut(&ACCOUNT).write(pack_account_state(0, 1, 0, 0, 0, 0)).unwrap();
            assert!(acc.is_initialized(ACCOUNT).unwrap());

            // Epoch-only: an IncrementLocalEpoch reset local_sequence to 0 while
            // bumping local_epoch. A non-zero epoch alone still marks initialized.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state_epoch(0, 0, 1, 0, 0, 0, 0))
                .unwrap();
            assert!(acc.is_initialized(ACCOUNT).unwrap());
        });
    }

    #[test]
    fn lock_status_distinguishes_locked_initiated_and_unlocked() {
        let delay = 3600u16;
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);

            // LOCK_OP: FLAG_LOCKED set, lock_union holds the delay. Hard-locked,
            // no unlock initiated — frozen regardless of `now`.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, Eip8130Constants::FLAG_LOCKED, delay as u64, 0, 0))
                .unwrap();
            assert!(acc.is_locked(ACCOUNT, 1_000).unwrap());
            let status = acc.get_lock_status(ACCOUNT, 1_000).unwrap();
            assert!(status.locked);
            assert!(!status.has_initiated_unlock);
            assert_eq!(status.unlocks_at, AccountState::UNLOCKS_AT_MAX);
            assert_eq!(status.unlock_delay, delay);

            // UNLOCK_OP: FLAG_UNLOCK_INITIATED set, lock_union holds unlocks_at.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(
                    0,
                    1,
                    Eip8130Constants::FLAG_LOCKED | Eip8130Constants::FLAG_UNLOCK_INITIATED,
                    2_000,
                    0,
                    0,
                ))
                .unwrap();
            assert!(acc.is_locked(ACCOUNT, 1_000).unwrap()); // before unlocks_at
            assert!(!acc.is_locked(ACCOUNT, 2_000).unwrap()); // at/after unlocks_at
            let status = acc.get_lock_status(ACCOUNT, 1_000).unwrap();
            assert!(status.locked);
            assert!(status.has_initiated_unlock);
            assert_eq!(status.unlocks_at, 2_000);

            // Never locked: no lock flags set.
            acc.account_state.at_mut(&ACCOUNT).write(pack_account_state(0, 1, 0, 0, 0, 0)).unwrap();
            assert!(!acc.is_locked(ACCOUNT, 0).unwrap());
            assert!(!acc.get_lock_status(ACCOUNT, 0).unwrap().has_initiated_unlock);
        });
    }

    #[test]
    fn actor_config_to_word_inverts_from_word_and_matches_packing() {
        let authenticator = address!("0x1234567890abcDEF1234567890aBcdef12345678");
        let config =
            ActorConfig::from_word(pack_actor_config(authenticator, 0xAB, (1u64 << 48) - 1));
        // to_word matches the independent Solidity packing, and round-trips.
        assert_eq!(config.to_word(), pack_actor_config(authenticator, 0xAB, (1u64 << 48) - 1));
        assert_eq!(ActorConfig::from_word(config.to_word()), config);
        assert_eq!(ActorConfig::EMPTY.to_word(), U256::ZERO);
    }

    #[test]
    fn actor_config_reserved_bits_are_detected_and_cleared_on_write() {
        // Reserved region is the top 4 bytes (bits 224..256), above the uint16
        // scope (bits 208..224).
        let word = pack_actor_config(ACCOUNT, 0xAB, 42) | (U256::from(1) << 224);
        assert!(ActorConfig::has_nonzero_reserved(word));
        let config = ActorConfig::from_word(word);
        assert!(!ActorConfig::has_nonzero_reserved(config.to_word()));
        assert_eq!(config.to_word(), pack_actor_config(ACCOUNT, 0xAB, 42));
    }

    #[test]
    fn account_state_to_word_inverts_from_word_and_matches_packing() {
        let word = pack_account_state(
            7,
            3,
            Eip8130Constants::DEFAULT_EOA_REVOKED | Eip8130Constants::FLAG_UNLOCK_INITIATED,
            (1u64 << 48) - 1,
            0xAB,
            (1u64 << 48) - 1,
        );
        let state = AccountState::from_word(word);
        assert_eq!(state.to_word(), word);
        assert_eq!(AccountState::from_word(state.to_word()), state);
    }

    #[test]
    fn account_state_splits_local_epoch_from_sequence() {
        // The local word is `localEpoch(32) || localSequence(32)`; each half must
        // unpack independently, and a full-width uint48 lock union must survive.
        let lock_union = (1u64 << 48) - 1; // full uint48
        let word = pack_account_state_epoch(
            9,
            0x1234_5678,
            0x0000_00ab,
            Eip8130Constants::FLAG_LOCKED | Eip8130Constants::FLAG_UNLOCK_INITIATED,
            lock_union,
            Eip8130Constants::SCOPE_POLICY,
            (1u64 << 48) - 1,
        );
        let state = AccountState::from_word(word);
        assert_eq!(state.multichain_sequence, 9);
        assert_eq!(state.local_sequence, 0x1234_5678);
        assert_eq!(state.local_epoch, 0x0000_00ab);
        assert_eq!(state.lock_union, lock_union);
        assert_eq!(state.default_eoa_scope, Eip8130Constants::SCOPE_POLICY);
        assert_eq!(state.to_word(), word);
    }

    #[test]
    fn self_actor_id_right_aligns_the_address() {
        let id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
        assert_eq!(&id.as_slice()[12..], ACCOUNT.as_slice());
        assert_eq!(&id.as_slice()[..12], &[0u8; 12]);
    }

    /// Packs an `ActorConfig` carrying only an authenticator (scope/expiry/policy
    /// zero) — the common shape for the `is_actor` predicate.
    fn pack(authenticator: Address) -> U256 {
        pack_actor_config(authenticator, 0, 0)
    }
}
