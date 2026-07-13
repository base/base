//! Native read-only mirror of the EIP-8130 `AccountConfiguration` system
//! contract's storage layout and its storage-view functions.

use alloy_primitives::{Address, B256, U256};
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, Result};

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

    /// Returns the [`ActorConfig`] for `(account, actor_id)`. An absent actor
    /// reads back as an all-zero word, i.e. [`ActorConfig::EMPTY`].
    pub fn get_actor_config(&self, account: Address, actor_id: B256) -> Result<ActorConfig> {
        Ok(ActorConfig::from_word(self.actor_config.at(&actor_id).at(&account).read()?))
    }

    /// Mirrors `AccountConfiguration.isActor`: `true` for any explicit
    /// `actor_config` entry (a non-empty authenticator), or the secp256k1 self
    /// key (the `actor_id` is the account, with no explicit entry) while its
    /// `DEFAULT_EOA_REVOKED` flag is unset. A live *scoped* self has no explicit
    /// entry — its config lives inline in `AccountState` — and so resolves
    /// through this same self path.
    pub fn is_actor(&self, account: Address, actor_id: B256) -> Result<bool> {
        // An explicit entry (any non-empty authenticator) is always a live actor.
        if self.get_actor_config(account, actor_id)?.authenticator != Address::ZERO {
            return Ok(true);
        }
        // No explicit entry: the self-actor is the secp256k1 self key (full owner
        // or inline-scoped), live unless the `DEFAULT_EOA_REVOKED` flag is set.
        if actor_id == Self::self_actor_id(account) {
            return Ok(!self.get_account_state(account)?.default_eoa_revoked());
        }
        Ok(false)
    }

    /// Mirrors `AccountConfiguration.getPolicy`: resolves an actor's policy
    /// gate target and signed commitment. An ungated actor resolves to
    /// `(address(0), bytes32(0))`; a gated one to `(manager, commitment)`.
    /// The secp256k1 self key's policy scope lives inline in
    /// `AccountState` (read only while the self key is live); a non-k1 self and
    /// every other actor resolve from `actor_config`.
    pub fn get_policy(&self, account: Address, actor_id: B256) -> Result<(Address, B256)> {
        let stored = self.get_actor_config(account, actor_id)?;
        let scope = if stored.authenticator != Address::ZERO {
            stored.scope
        } else if actor_id == Self::self_actor_id(account) {
            let state = self.get_account_state(account)?;
            if state.default_eoa_revoked() {
                return Ok((Address::ZERO, B256::ZERO));
            }
            state.default_eoa_scope
        } else {
            return Ok((Address::ZERO, B256::ZERO));
        };
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
    /// 8130 transaction against the actor's signed commitment. The
    /// `_authorizeActor`/`_revokeActor` invariant is that this slot is non-zero
    /// iff the actor has `SCOPE_POLICY` (across both self homes), so a
    /// zero return unambiguously means "no policy / no actor". Mirrors
    /// `AccountConfiguration.getPolicyCommitment`.
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

    /// `true` once the account is initialized (created or imported); the contract
    /// uses `local_sequence > 0` as the initialized flag.
    pub fn is_initialized(&self, account: Address) -> Result<bool> {
        Ok(self.get_account_state(account)?.local_sequence > 0)
    }

    /// Mirrors `AccountConfiguration._isLocked`: not locked unless `FLAG_LOCKED`
    /// is set; hard-locked (frozen) while `FLAG_UNLOCK_INITIATED` is clear; once
    /// an unlock is initiated, frozen only until `now >= lock_union` (`unlocks_at`).
    /// `now` is supplied by the caller (block timestamp at inclusion, wall-clock
    /// in the pool), since the reader has no block context.
    pub fn is_locked(&self, account: Address, now: u64) -> Result<bool> {
        Ok(self.get_account_state(account)?.is_locked(now))
    }

    /// Mirrors `AccountConfiguration.getLockStatus`.
    pub fn get_lock_status(&self, account: Address, now: u64) -> Result<LockStatus> {
        Ok(self.get_account_state(account)?.lock_status(now))
    }

    /// The implicit-EOA self-actor id for `account`: `bytes32(bytes20(account))`,
    /// i.e. the address left-aligned in the high 20 bytes.
    #[must_use]
    pub fn self_actor_id(account: Address) -> B256 {
        let mut word = [0u8; 32];
        word[..20].copy_from_slice(account.as_slice());
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

/// Decoded `AccountConfiguration.ActorConfig` (one packed storage slot).
///
/// Solidity layout `{address authenticator; uint8 scope; uint48 expiry;}` packs
/// right-aligned in declaration order, lowest-order field
/// first, into a single 32-byte slot:
///
/// ```text
/// bytes (big-endian):  [0..5) reserved | [5..11) expiry | [11] scope | [12..32) authenticator
/// bits  (LSB-first):   authenticator 0..160 | scope 160..168 | expiry 168..216 | reserved 216..256
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActorConfig {
    /// Authenticator address bound to the actor (`address(0)` = empty slot,
    /// `address(1)` = native k1/ecrecover, any other = `IAuthenticator` contract).
    pub authenticator: Address,
    /// Elevated-scope bitfield (`0 = unrestricted`).
    pub scope: u8,
    /// Unix-seconds expiry; `0 = no expiry`. The actor is invalid once
    /// `block.timestamp > expiry`.
    pub expiry: u64,
}

impl ActorConfig {
    /// The empty (unset) actor config: a zeroed storage slot.
    pub const EMPTY: Self = Self { authenticator: Address::ZERO, scope: 0, expiry: 0 };

    /// Returns whether the reserved high 40 bits of a packed word are non-zero.
    #[must_use]
    pub fn has_nonzero_reserved(word: U256) -> bool {
        word.to_be_bytes::<32>()[..5].iter().any(|&byte| byte != 0)
    }

    /// Unpacks a raw `ActorConfig` storage word.
    #[must_use]
    pub fn from_word(word: U256) -> Self {
        let b = word.to_be_bytes::<32>();
        let mut expiry = [0u8; 8];
        expiry[2..].copy_from_slice(&b[5..11]); // uint48: 6 bytes, big-endian
        Self {
            authenticator: Address::from_slice(&b[12..32]),
            scope: b[11],
            expiry: u64::from_be_bytes(expiry),
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
        b[11] = self.scope;
        b[5..11].copy_from_slice(&self.expiry.to_be_bytes()[2..]); // uint48: low 6 bytes
        U256::from_be_bytes(b)
    }
}

/// Decoded `AccountConfiguration.AccountState` (one packed storage slot).
///
/// Solidity layout `{uint64 multichainSequence; uint64 localSequence; uint8
/// flags; uint40 lockUnion; uint8 defaultEOAScope; uint48 defaultEOAExpiry;}`,
/// packed right-aligned, lowest-order field first; the top 3 bytes of the slot
/// are reserved padding that MUST stay zero:
///
/// ```text
/// bits (LSB-first): multichain 0..64 | local 64..128 | flags 128..136 | lock_union 136..176 | defaultEOAScope 176..184 | defaultEOAExpiry 184..232 | reserved 232..256
/// ```
///
/// `lock_union` is a `uint40` union field (see [Account Lock] in the spec): while
/// [`Eip8130Constants::FLAG_UNLOCK_INITIATED`] is clear it holds the configured
/// `unlock_delay` (seconds, `uint16` range); while set it holds `unlocks_at` (the
/// timestamp at which a pending unlock takes effect). Lock state is mutated only
/// through the EVM `applySignedLockChanges` entry point; the native path only
/// reads it (see [`Self::is_locked`]).
///
/// The `default_eoa_*` fields are the inline home for the account's own
/// secp256k1 ("self") key: when `DEFAULT_EOA_REVOKED` is unset, a k1 signature
/// recovering to the account authenticates with this inline config — all-zero
/// is the implicit full owner, a non-zero scope/policy/expiry is a scoped self
/// — so the entire self key resolves in a single account-state SLOAD. The
/// `actor_config(self)` slot is reserved for a *non*-k1 self authenticator
/// (e.g. a post-quantum verifier returning the self-actorId); the inline k1
/// self and a non-k1 self are mutually exclusive.
///
/// [Account Lock]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountState {
    /// Sequence for `chain_id == 0` (multichain) signed actor changes.
    pub multichain_sequence: u64,
    /// Sequence for local (`chain_id == block.chainid`) changes; `> 0` also marks
    /// the account initialized.
    pub local_sequence: u64,
    /// Account flags bitfield: bit 0 ([`Eip8130Constants::DEFAULT_EOA_REVOKED`])
    /// disables the inline secp256k1 self key; bit 1
    /// ([`Eip8130Constants::FLAG_LOCKED`]) freezes actor configuration; bit 2
    /// ([`Eip8130Constants::FLAG_UNLOCK_INITIATED`]) selects the `lock_union`
    /// interpretation.
    pub flags: u8,
    /// `uint40` lock union: `unlock_delay` (seconds) while `FLAG_UNLOCK_INITIATED`
    /// is clear, else `unlocks_at` (Unix-seconds timestamp).
    pub lock_union: u64,
    /// Inline self-key scope bitfield (`0` = unrestricted full owner). Governs
    /// only when the self key is live (`!default_eoa_revoked()`).
    pub default_eoa_scope: u8,
    /// Inline self-key Unix-seconds expiry (`0` = no expiry). The self key is
    /// invalid once `now > default_eoa_expiry`.
    pub default_eoa_expiry: u64,
}

impl AccountState {
    /// `type(uint40).max` — the `unlocks_at` value `getLockStatus` synthesizes for
    /// a hard-locked account (`FLAG_LOCKED` set, `FLAG_UNLOCK_INITIATED` clear),
    /// where `lock_union` actually stores the configured delay rather than a
    /// timestamp. Not a stored sentinel.
    pub const UNLOCKS_AT_MAX: u64 = (1 << 40) - 1;

    /// Unpacks a raw `AccountState` storage word.
    #[must_use]
    pub fn from_word(word: U256) -> Self {
        let b = word.to_be_bytes::<32>();
        let mut multichain = [0u8; 8];
        let mut local = [0u8; 8];
        let mut lock_union = [0u8; 8];
        let mut default_eoa_expiry = [0u8; 8];
        multichain.copy_from_slice(&b[24..32]); // uint64 at bits 0..64
        local.copy_from_slice(&b[16..24]); // uint64 at bits 64..128
        lock_union[3..].copy_from_slice(&b[10..15]); // uint40 at bits 136..176
        default_eoa_expiry[2..].copy_from_slice(&b[3..9]); // uint48 at bits 184..232
        Self {
            multichain_sequence: u64::from_be_bytes(multichain),
            local_sequence: u64::from_be_bytes(local),
            flags: b[15], // uint8 at bits 128..136
            lock_union: u64::from_be_bytes(lock_union),
            default_eoa_scope: b[9], // uint8 at bits 176..184
            default_eoa_expiry: u64::from_be_bytes(default_eoa_expiry),
        }
    }

    /// `true` when the implicit default-EOA path is disabled for this account
    /// (the `DEFAULT_EOA_REVOKED` flag bit is set).
    #[must_use]
    pub const fn default_eoa_revoked(&self) -> bool {
        self.flags & Eip8130Constants::DEFAULT_EOA_REVOKED != 0
    }

    /// Mirrors `AccountConfiguration._isLocked`: configuration is frozen while
    /// `FLAG_LOCKED` is set, except once an unlock has been initiated
    /// (`FLAG_UNLOCK_INITIATED`), when it stays frozen only until `now` reaches
    /// the stored `unlocks_at` (`lock_union`).
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

    /// Packs this state into its raw storage word — the exact inverse of
    /// [`Self::from_word`].
    ///
    /// `lock_union` must fit in `uint40` and `default_eoa_expiry` in `uint48`
    /// (their storage field widths); higher bytes are dropped. Values sourced
    /// from [`Self::from_word`] or ABI decoding always satisfy this, so the
    /// `debug_assert!`s only guard hand-constructed misuse.
    #[must_use]
    pub fn to_word(&self) -> U256 {
        debug_assert!(self.lock_union >> 40 == 0, "lock_union exceeds uint40 storage width");
        debug_assert!(
            self.default_eoa_expiry >> 48 == 0,
            "default_eoa_expiry exceeds uint48 storage width"
        );
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&self.multichain_sequence.to_be_bytes());
        b[16..24].copy_from_slice(&self.local_sequence.to_be_bytes());
        b[15] = self.flags;
        b[10..15].copy_from_slice(&self.lock_union.to_be_bytes()[3..]); // uint40: low 5 bytes
        b[9] = self.default_eoa_scope;
        b[3..9].copy_from_slice(&self.default_eoa_expiry.to_be_bytes()[2..]); // uint48 at bits 184..232
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
    fn pack_actor_config(authenticator: Address, scope: u8, expiry: u64) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(scope) << 160)
            | (U256::from(expiry) << 168)
    }

    fn pack_account_state(
        multichain: u64,
        local: u64,
        flags: u8,
        lock_union: u64,
        default_eoa_scope: u8,
        default_eoa_expiry: u64,
    ) -> U256 {
        U256::from(multichain)
            | (U256::from(local) << 64)
            | (U256::from(flags) << 128)
            | (U256::from(lock_union) << 136)
            | (U256::from(default_eoa_scope) << 176)
            | (U256::from(default_eoa_expiry) << 184)
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
            let config = acc.get_actor_config(ACCOUNT, ACTOR).unwrap();
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
            let config = AccountConfigurationStorage::new(ctx).get_actor_config(ACCOUNT, ACTOR);
            assert_eq!(config.unwrap(), ActorConfig::EMPTY);
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
        let lock_union = (1u64 << 40) - 1; // full uint40
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
        let word = pack_actor_config(ACCOUNT, 0xAB, 42) | (U256::from(1) << 216);
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
            (1u64 << 40) - 1,
            0xAB,
            (1u64 << 48) - 1,
        );
        let state = AccountState::from_word(word);
        assert_eq!(state.to_word(), word);
        assert_eq!(AccountState::from_word(state.to_word()), state);
    }

    #[test]
    fn self_actor_id_left_aligns_the_address() {
        let id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
        assert_eq!(&id.as_slice()[..20], ACCOUNT.as_slice());
        assert_eq!(&id.as_slice()[20..], &[0u8; 12]);
    }

    /// Packs an `ActorConfig` carrying only an authenticator (scope/expiry/policy
    /// zero) — the common shape for the `is_actor` predicate.
    fn pack(authenticator: Address) -> U256 {
        pack_actor_config(authenticator, 0, 0)
    }
}
