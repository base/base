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
    /// slot 1: per-actor signed policy commitment (set when `policy_type != 0`).
    pub policy_commitment: Mapping<B256, Mapping<Address, B256>>,
    /// slot 2: per-actor policy manager (set when `policy_type != 0`).
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

    /// Mirrors `AccountConfiguration.isActor`: `true` for any actor bound to a
    /// real authenticator (`authenticator >= ECRECOVER && != REVOKED`), or the
    /// implicit-EOA self-actor (empty slot whose `actor_id` is the account).
    pub fn is_actor(&self, account: Address, actor_id: B256) -> Result<bool> {
        let authenticator = self.get_actor_config(account, actor_id)?.authenticator;
        if authenticator >= Eip8130Constants::ECRECOVER_AUTHENTICATOR
            && authenticator != Eip8130Constants::REVOKED_AUTHENTICATOR
        {
            return Ok(true);
        }
        Ok(authenticator == Address::ZERO && actor_id == Self::self_actor_id(account))
    }

    /// Mirrors `AccountConfiguration.getPolicy`: an ungated actor
    /// (`policy_type == 0`) resolves to `(address(0), bytes32(0))`; otherwise the
    /// stored `(manager, commitment)`.
    pub fn get_policy(&self, account: Address, actor_id: B256) -> Result<(Address, B256)> {
        if self.get_actor_config(account, actor_id)?.policy_type == 0 {
            return Ok((Address::ZERO, B256::ZERO));
        }
        let manager = self.policy_manager.at(&actor_id).at(&account).read()?;
        let commitment = self.policy_commitment.at(&actor_id).at(&account).read()?;
        Ok((manager, commitment))
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

    /// Mirrors `AccountConfiguration.isLocked`: locked while `now < unlocks_at`.
    /// `now` is supplied by the caller (block timestamp at inclusion, wall-clock
    /// in the pool), since the reader has no block context.
    pub fn is_locked(&self, account: Address, now: u64) -> Result<bool> {
        Ok(now < self.get_account_state(account)?.unlocks_at)
    }

    /// Mirrors `AccountConfiguration.getLockStatus`.
    pub fn get_lock_status(&self, account: Address, now: u64) -> Result<LockStatus> {
        let state = self.get_account_state(account)?;
        Ok(LockStatus {
            locked: now < state.unlocks_at,
            has_initiated_unlock: state.unlocks_at != 0
                && state.unlocks_at != AccountState::UNLOCKS_AT_MAX,
            unlocks_at: state.unlocks_at,
            unlock_delay: state.unlock_delay,
        })
    }

    /// The implicit-EOA self-actor id for `account`: `bytes32(bytes20(account))`,
    /// i.e. the address left-aligned in the high 20 bytes.
    #[must_use]
    pub fn self_actor_id(account: Address) -> B256 {
        let mut word = [0u8; 32];
        word[..20].copy_from_slice(account.as_slice());
        B256::from(word)
    }
}

/// Decoded `AccountConfiguration.ActorConfig` (one packed storage slot).
///
/// Solidity layout `{address authenticator; uint8 scope; uint48 expiry; uint8
/// policyType;}` packs right-aligned in declaration order, lowest-order field
/// first, into a single 32-byte slot:
///
/// ```text
/// bytes (big-endian):  [0..4) unused | [4] policyType | [5..11) expiry | [11] scope | [12..32) authenticator
/// bits  (LSB-first):   authenticator 0..160 | scope 160..168 | expiry 168..216 | policyType 216..224
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActorConfig {
    /// Authenticator address bound to the actor (`address(0)` = empty slot,
    /// `address(1)` = native ecrecover, `address(uint160).max` = revoked).
    pub authenticator: Address,
    /// Elevated-scope bitfield (`0 = unrestricted`).
    pub scope: u8,
    /// Unix-seconds expiry; `0 = no expiry`. The actor is invalid once
    /// `block.timestamp > expiry`.
    pub expiry: u64,
    /// Policy sub-type byte; `0 = ungated`.
    pub policy_type: u8,
}

impl ActorConfig {
    /// The empty (unset) actor config: a zeroed storage slot.
    pub const EMPTY: Self =
        Self { authenticator: Address::ZERO, scope: 0, expiry: 0, policy_type: 0 };

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
            policy_type: b[4],
        }
    }

    /// `true` if the slot is empty (no authenticator bound).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.authenticator == Address::ZERO
    }
}

/// Decoded `AccountConfiguration.AccountState` (one packed storage slot).
///
/// Solidity layout `{uint64 multichainSequence; uint64 localSequence; uint40
/// unlocksAt; uint16 unlockDelay;}`, packed right-aligned, lowest-order field
/// first:
///
/// ```text
/// bits (LSB-first): multichain 0..64 | local 64..128 | unlocksAt 128..168 | unlockDelay 168..184
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountState {
    /// Sequence for `chain_id == 0` (multichain) signed actor changes.
    pub multichain_sequence: u64,
    /// Sequence for local (`chain_id == block.chainid`) changes; `> 0` also marks
    /// the account initialized.
    pub local_sequence: u64,
    /// Unlock timestamp (`uint40`). Locked while `now < unlocks_at`;
    /// [`Self::UNLOCKS_AT_MAX`] means locked with no unlock initiated.
    pub unlocks_at: u64,
    /// Unlock delay in seconds.
    pub unlock_delay: u16,
}

impl AccountState {
    /// `type(uint40).max` — the sentinel `unlocks_at` written by `lock()` before
    /// an unlock is initiated (locked indefinitely).
    pub const UNLOCKS_AT_MAX: u64 = (1 << 40) - 1;

    /// Unpacks a raw `AccountState` storage word.
    #[must_use]
    pub fn from_word(word: U256) -> Self {
        let b = word.to_be_bytes::<32>();
        let mut multichain = [0u8; 8];
        let mut local = [0u8; 8];
        let mut unlocks_at = [0u8; 8];
        let mut unlock_delay = [0u8; 2];
        multichain.copy_from_slice(&b[24..32]); // uint64
        local.copy_from_slice(&b[16..24]); // uint64
        unlocks_at[3..].copy_from_slice(&b[11..16]); // uint40: 5 bytes, big-endian
        unlock_delay.copy_from_slice(&b[9..11]); // uint16
        Self {
            multichain_sequence: u64::from_be_bytes(multichain),
            local_sequence: u64::from_be_bytes(local),
            unlocks_at: u64::from_be_bytes(unlocks_at),
            unlock_delay: u16::from_be_bytes(unlock_delay),
        }
    }
}

/// Decoded result of `AccountConfiguration.getLockStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LockStatus {
    /// `true` while `now < unlocks_at`.
    pub locked: bool,
    /// `true` once `initiateUnlock` has run (`unlocks_at` set to a real time, not
    /// `0` and not [`AccountState::UNLOCKS_AT_MAX`]).
    pub has_initiated_unlock: bool,
    /// The stored unlock timestamp.
    pub unlocks_at: u64,
    /// The stored unlock delay in seconds.
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
    fn pack_actor_config(authenticator: Address, scope: u8, expiry: u64, policy_type: u8) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(scope) << 160)
            | (U256::from(expiry) << 168)
            | (U256::from(policy_type) << 216)
    }

    fn pack_account_state(multichain: u64, local: u64, unlocks_at: u64, unlock_delay: u16) -> U256 {
        U256::from(multichain)
            | (U256::from(local) << 64)
            | (U256::from(unlocks_at) << 128)
            | (U256::from(unlock_delay) << 168)
    }

    #[test]
    fn actor_config_unpacks_each_field_from_its_slot_position() {
        let authenticator = address!("0x1234567890abcDEF1234567890aBcdef12345678");
        let expiry = (1u64 << 48) - 1; // full uint48
        let word = pack_actor_config(authenticator, 0xAB, expiry, 0xCD);

        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(word).unwrap();
            let config = acc.get_actor_config(ACCOUNT, ACTOR).unwrap();
            assert_eq!(config.authenticator, authenticator);
            assert_eq!(config.scope, 0xAB);
            assert_eq!(config.expiry, expiry);
            assert_eq!(config.policy_type, 0xCD);
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
        let revoked = Eip8130Constants::REVOKED_AUTHENTICATOR;
        let bound = address!("0x00000000000000000000000000000000000000ff");
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);

            // Bound to a real authenticator -> actor.
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(pack(bound)).unwrap();
            assert!(acc.is_actor(ACCOUNT, ACTOR).unwrap());

            // Revoked sentinel -> not an actor.
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(pack(revoked)).unwrap();
            assert!(!acc.is_actor(ACCOUNT, ACTOR).unwrap());

            // Empty slot, non-self actor id -> not an actor.
            let other = b256!("0x00000000000000000000000000000000000000cc000000000000000000000000");
            assert!(!acc.is_actor(ACCOUNT, other).unwrap());

            // Empty slot, self actor id -> implicit EOA actor.
            let self_id = AccountConfigurationStorage::self_actor_id(ACCOUNT);
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

            // Ungated actor (policy_type 0) -> zeroed regardless of stored slots.
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(pack(manager)).unwrap();
            acc.policy_manager.at_mut(&ACTOR).at_mut(&ACCOUNT).write(manager).unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, ACTOR).unwrap(), (Address::ZERO, B256::ZERO));

            // Gated actor -> (manager, commitment).
            let gated = pack_actor_config(manager, 0, 0, 1);
            acc.actor_config.at_mut(&ACTOR).at_mut(&ACCOUNT).write(gated).unwrap();
            acc.policy_commitment.at_mut(&ACTOR).at_mut(&ACCOUNT).write(commitment).unwrap();
            assert_eq!(acc.get_policy(ACCOUNT, ACTOR).unwrap(), (manager, commitment));
        });
    }

    #[test]
    fn account_state_unpacks_sequences_and_lock_fields() {
        let word = pack_account_state(7, 3, (1u64 << 40) - 1, 0xBEEF);
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut acc = AccountConfigurationStorage::new(ctx);
            acc.account_state.at_mut(&ACCOUNT).write(word).unwrap();

            let state = acc.get_account_state(ACCOUNT).unwrap();
            assert_eq!(state.multichain_sequence, 7);
            assert_eq!(state.local_sequence, 3);
            assert_eq!(state.unlocks_at, AccountState::UNLOCKS_AT_MAX);
            assert_eq!(state.unlock_delay, 0xBEEF);
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

            // lock(): unlocks_at = max, delay set. Locked, no unlock initiated.
            acc.account_state
                .at_mut(&ACCOUNT)
                .write(pack_account_state(0, 1, AccountState::UNLOCKS_AT_MAX, delay))
                .unwrap();
            assert!(acc.is_locked(ACCOUNT, 1_000).unwrap());
            let status = acc.get_lock_status(ACCOUNT, 1_000).unwrap();
            assert!(status.locked);
            assert!(!status.has_initiated_unlock);
            assert_eq!(status.unlock_delay, delay);

            // initiateUnlock(): unlocks_at = real future time, delay cleared.
            acc.account_state.at_mut(&ACCOUNT).write(pack_account_state(0, 1, 2_000, 0)).unwrap();
            assert!(acc.is_locked(ACCOUNT, 1_000).unwrap()); // before unlocks_at
            assert!(!acc.is_locked(ACCOUNT, 2_000).unwrap()); // at/after unlocks_at
            let status = acc.get_lock_status(ACCOUNT, 1_000).unwrap();
            assert!(status.locked);
            assert!(status.has_initiated_unlock);
            assert_eq!(status.unlocks_at, 2_000);

            // Never locked: unlocks_at = 0.
            acc.account_state.at_mut(&ACCOUNT).write(pack_account_state(0, 1, 0, 0)).unwrap();
            assert!(!acc.is_locked(ACCOUNT, 0).unwrap());
            assert!(!acc.get_lock_status(ACCOUNT, 0).unwrap().has_initiated_unlock);
        });
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
        pack_actor_config(authenticator, 0, 0, 0)
    }
}
