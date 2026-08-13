//! Storage layout and constants for the EIP-8130 2D nonce manager precompile.

use alloy_primitives::{Address, B256, U256, address};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Handler, Mapping, Result, StorageKey};

use crate::INonceManager;

/// Persistent 2D nonce manager for EIP-8130 account-abstraction transactions.
///
/// Mirrors a Solidity contract with the layout:
///
/// ```solidity
/// contract NonceManager {
///     mapping(address => mapping(uint256 => uint64)) nonces;  // slot 0
///     mapping(bytes32 => uint64) expiringNonceSeen;           // slot 1: replay hash => expiry
///     mapping(uint32 => bytes32) expiringNonceRing;           // slot 2: circular buffer
///     uint32 expiringNonceRingPtr;                            // slot 3: ring write position
/// }
/// ```
///
/// The 2D nonce (`nonces`) provides sequence-nonce channels keyed by an
/// arbitrary `nonce_key`, allowing a sender to have many independently-ordered
/// in-flight transactions. Nonce key `0` is the protocol nonce and lives in
/// account state, not here.
///
/// The expiring-nonce structures provide replay protection for nonce-free
/// (`NONCE_KEY_MAX`) transactions: a signature-invariant replay hash is recorded
/// with its expiry in a fixed-capacity circular buffer that reclaims expired
/// slots as the write pointer advances.
#[contract(addr = Self::ADDRESS)]
#[namespace("base.nonce_manager")]
pub struct NonceManagerStorage {
    /// 2D sequence nonces keyed by `(account, nonce_key)`.
    pub nonces: Mapping<Address, Mapping<U256, u64>>,
    /// Expiring-nonce seen set: replay hash => `valid_before` expiry timestamp.
    pub expiring_nonce_seen: Mapping<B256, u64>,
    /// Circular buffer of recorded replay hashes, indexed by ring position.
    pub expiring_nonce_ring: Mapping<u32, B256>,
    /// Current circular-buffer write position; wraps at
    /// [`Self::REPLAY_BUFFER_CAPACITY`].
    pub expiring_nonce_ring_ptr: u32,
}

impl NonceManagerStorage<'_> {
    /// 2D nonce manager precompile address.
    ///
    /// Pinned to `NONCE_MANAGER_ADDRESS` from the EIP-8130 constant table
    /// (`0x8130…aa01`, in the `0x8130…` / EIP-number namespace for EIP-8130
    /// system precompiles).
    pub const ADDRESS: Address = address!("813000000000000000000000000000000000aa01");

    /// Base storage slot of the `nonces` mapping under this contract's
    /// ERC-7201 namespace.
    ///
    /// Re-exported from the macro-generated `slots` module so off-chain
    /// readers (e.g. RPC) can derive `nonces[account][nonce_key]` slots
    /// without instantiating the precompile. Pair with [`Self::nonce_slot`].
    pub const NONCES_BASE_SLOT: U256 = slots::NONCES;
    /// Base storage slot of the nonce-free replay mapping.
    pub const EXPIRING_NONCE_SEEN_BASE_SLOT: U256 = slots::EXPIRING_NONCE_SEEN;

    /// Fixed capacity of the nonce-free `replay_id` ring buffer
    /// (`REPLAY_BUFFER_CAPACITY` in the EIP-8130 constant table).
    ///
    /// A consensus chain parameter: identical for every node on the chain, not a
    /// per-node choice. Sized together with [`Self::NONCE_FREE_EXPIRY_WINDOW`] so
    /// that `peak accepted nonce-free throughput × NONCE_FREE_EXPIRY_WINDOW` stays
    /// within capacity (~10k TPS for ~30s), so that by the time the write pointer
    /// wraps back to a slot, the entry it holds has expired and can be reclaimed.
    pub const REPLAY_BUFFER_CAPACITY: u32 = 300_000;

    /// Maximum validity-window span accepted for a nonce-free transaction, in
    /// **milliseconds** (`NONCE_FREE_EXPIRY_WINDOW` in the EIP-8130 constant
    /// table).
    ///
    /// A consensus chain parameter (not a per-node choice), sized together with
    /// [`Self::REPLAY_BUFFER_CAPACITY`]. A transaction's `valid_before` (Unix
    /// milliseconds) must fall in `(now, now + this]`, where `now` is
    /// `block.timestamp * 1000`. 30,000 ms == 30 s, so the buffer sizing below is
    /// unchanged by the seconds→milliseconds move.
    pub const NONCE_FREE_EXPIRY_WINDOW: u64 = 30_000;

    /// Nonce key reserved for the protocol nonce, which is held in account state.
    const PROTOCOL_NONCE_KEY: U256 = U256::ZERO;

    /// Returns the current 2D nonce for `account` at `nonce_key`.
    ///
    /// # Errors
    /// - [`INonceManager::ProtocolNonceNotSupported`] — `nonce_key` is `0`, the
    ///   protocol nonce, which is stored in account state and must be read from there.
    pub fn get_nonce(&self, account: Address, nonce_key: U256) -> Result<u64> {
        if nonce_key == Self::PROTOCOL_NONCE_KEY {
            return Err(BasePrecompileError::revert(INonceManager::ProtocolNonceNotSupported {}));
        }
        self.nonces.at(&account).at(&nonce_key).read()
    }

    /// Returns the EVM storage slot that holds the 2D channel nonce for
    /// `nonces[account][nonce_key]`.
    ///
    /// Off-chain dual of [`Self::get_nonce`]: same preconditions, same error
    /// for `nonce_key == 0`, but does not read storage. Intended for off-chain
    /// readers (e.g. an RPC `eth_getTransactionCount` extension) that want to
    /// look up a channel nonce via `storage_at` without instantiating the
    /// precompile. The decoded value at this slot is a `u64` right-aligned in
    /// the slot's low 8 bytes.
    ///
    /// # Errors
    /// - [`INonceManager::ProtocolNonceNotSupported`] — `nonce_key` is `0`, the
    ///   protocol nonce, which is stored in account state and must be read from there.
    pub fn nonce_slot(account: Address, nonce_key: U256) -> Result<U256> {
        if nonce_key == Self::PROTOCOL_NONCE_KEY {
            return Err(BasePrecompileError::revert(INonceManager::ProtocolNonceNotSupported {}));
        }
        Ok(nonce_key.mapping_slot(account.mapping_slot(Self::NONCES_BASE_SLOT)))
    }

    /// Returns the storage slot holding the expiry recorded for `replay_id`.
    pub fn expiring_nonce_seen_slot(replay_id: B256) -> U256 {
        U256::from_be_bytes(replay_id.0).mapping_slot(Self::EXPIRING_NONCE_SEEN_BASE_SLOT)
    }

    /// Increments the 2D nonce for `account` at `nonce_key`, returning the new
    /// value and emitting [`INonceManager::NonceIncremented`].
    ///
    /// Intended for the EIP-8130 execution layer; not reachable through ABI
    /// dispatch.
    ///
    /// # Errors
    /// - [`INonceManager::InvalidNonceKey`] — `nonce_key` is `0` (the protocol nonce).
    /// - [`INonceManager::NonceOverflow`] — the channel nonce is already `u64::MAX`.
    pub fn increment_nonce(&mut self, account: Address, nonce_key: U256) -> Result<u64> {
        if nonce_key == Self::PROTOCOL_NONCE_KEY {
            return Err(BasePrecompileError::revert(INonceManager::InvalidNonceKey {}));
        }

        let current = self.nonces.at(&account).at(&nonce_key).read()?;
        self.increment_nonce_from_current(account, nonce_key, current)
    }

    /// Increments a 2D nonce from its caller-provided current value.
    ///
    /// `current` must be loaded from the same storage context with no intervening
    /// write to `(account, nonce_key)`.
    ///
    /// # Errors
    /// - [`INonceManager::InvalidNonceKey`] — `nonce_key` is `0` (the protocol nonce).
    /// - [`INonceManager::NonceOverflow`] — `current` is already `u64::MAX`.
    pub fn increment_nonce_from_current(
        &mut self,
        account: Address,
        nonce_key: U256,
        current: u64,
    ) -> Result<u64> {
        if nonce_key == Self::PROTOCOL_NONCE_KEY {
            return Err(BasePrecompileError::revert(INonceManager::InvalidNonceKey {}));
        }

        // The nonce write and its NonceIncremented event must commit together;
        // guard them with a checkpoint so a failure after the write (e.g. during
        // event emission) reverts the advanced nonce rather than leaving it
        // advanced without a log. The guard reverts on drop unless committed.
        let checkpoint = self.storage.checkpoint();

        self.__initialize()?;
        let new_nonce = current
            .checked_add(1)
            .ok_or_else(|| BasePrecompileError::revert(INonceManager::NonceOverflow {}))?;
        self.nonces.at_mut(&account).at_mut(&nonce_key).write(new_nonce)?;
        self.emit_event(INonceManager::NonceIncremented {
            account,
            nonceKey: nonce_key,
            newNonce: new_nonce,
        })?;

        checkpoint.commit();
        Ok(new_nonce)
    }

    /// Returns whether `hash` has been recorded and has not yet expired relative
    /// to `now` (Unix **milliseconds**).
    ///
    /// Intended for transaction-pool replay checks. `now` is a caller-supplied
    /// timestamp (in milliseconds) because the mempool has no block context and
    /// uses wall-clock time, whereas [`Self::check_and_mark_expiring_nonce`] reads
    /// the block timestamp internally at inclusion. The two clocks can disagree
    /// near an entry's expiry boundary; this getter is an advisory pre-filter and
    /// the block-timestamp check at inclusion is authoritative.
    pub fn is_expiring_nonce_seen(&self, hash: B256, now: u64) -> Result<bool> {
        let expiry = self.expiring_nonce_seen.at(&hash).read()?;
        Ok(expiry != 0 && expiry > now)
    }

    /// Validates and records an expiring-nonce transaction, providing replay
    /// protection for nonce-free EIP-8130 transactions.
    ///
    /// `expiring_nonce_hash` is the canonical `TxEip8130::replay_id`:
    /// `keccak256(REPLAY_ID_TYPE || rlp([chain_id, resolved_sender, valid_after,
    /// valid_before, account_changes, calls, metadata, payer]))`. Fees, nonce
    /// fields, and authentication blobs are omitted, so fee-bumped or re-signed
    /// variants of one logical transaction collapse to a single entry.
    /// The hash is recorded in a circular buffer that reclaims expired slots as
    /// the write pointer advances.
    ///
    /// `valid_before` and the internal `now` are both Unix **milliseconds**;
    /// `now` is `block.timestamp * 1000`, so this is the authoritative
    /// inclusion-time replay check (cf. the advisory, wall-clock-based
    /// [`Self::is_expiring_nonce_seen`] used by the mempool).
    ///
    /// Intended for the EIP-8130 execution layer; not reachable through ABI
    /// dispatch.
    ///
    /// # Errors
    /// - [`INonceManager::InvalidExpiringNonceExpiry`] — `valid_before` is not in
    ///   `(now, now + NONCE_FREE_EXPIRY_WINDOW]`.
    /// - [`INonceManager::ExpiringNonceReplay`] — the hash is already recorded and unexpired.
    /// - [`INonceManager::ExpiringNonceSetFull`] — the ring slot holds an unexpired entry
    ///   that cannot be reclaimed.
    pub fn check_and_mark_expiring_nonce(
        &mut self,
        expiring_nonce_hash: B256,
        valid_before: u64,
    ) -> Result<()> {
        // Both `valid_before` and this `now` are Unix milliseconds; the EIP
        // evaluates the window against `block.timestamp * 1000`.
        let now: u64 = self.storage.timestamp().saturating_to::<u64>().saturating_mul(1_000);

        // 1. Validate the expiry window: must be in (now, now + MAX_EXPIRY_MS].
        if valid_before <= now || valid_before > now.saturating_add(Self::NONCE_FREE_EXPIRY_WINDOW)
        {
            return Err(BasePrecompileError::revert(INonceManager::InvalidExpiringNonceExpiry {}));
        }

        // 2. Replay check: reject if the hash is already seen and not yet expired.
        let seen_expiry = self.expiring_nonce_seen.at(&expiring_nonce_hash).read()?;
        if seen_expiry != 0 && seen_expiry > now {
            return Err(BasePrecompileError::revert(INonceManager::ExpiringNonceReplay {}));
        }

        // Steps 3–6 mutate four correlated persistent slots (the reclaimed old
        // entry, the ring slot, the new seen entry, and the write pointer). Guard
        // them with a checkpoint so a mid-sequence failure reverts the whole group
        // atomically — otherwise a partial write could record an entry in
        // `expiring_nonce_seen` whose ring slot is never reclaimable because the
        // pointer never advanced. The guard reverts on drop unless committed, so
        // any early `?` return below rolls back every write in the group.
        let checkpoint = self.storage.checkpoint();

        self.__initialize()?;

        // 3. Inspect the entry the write pointer currently references.
        let ptr = self.expiring_nonce_ring_ptr.read()?;
        let old_hash = self.expiring_nonce_ring.at(&ptr).read()?;

        // 4. Reclaim the slot only if its occupant has expired (or is empty).
        // The buffer is sized so entries should always be expired by the time the
        // pointer wraps, but verify in case throughput exceeds expectations.
        if old_hash != B256::ZERO {
            let old_expiry = self.expiring_nonce_seen.at(&old_hash).read()?;
            if old_expiry != 0 && old_expiry > now {
                return Err(BasePrecompileError::revert(INonceManager::ExpiringNonceSetFull {}));
            }
            self.expiring_nonce_seen.at_mut(&old_hash).write(0)?;
        }

        // 5. Record the new entry.
        self.expiring_nonce_ring.at_mut(&ptr).write(expiring_nonce_hash)?;
        self.expiring_nonce_seen.at_mut(&expiring_nonce_hash).write(valid_before)?;

        // 6. Advance the write pointer, wrapping at capacity (not u32::MAX).
        // `wrapping_add` is defensive: a corrupted ptr at u32::MAX wraps to 0
        // (still < capacity) rather than panicking in debug builds.
        let incremented = ptr.wrapping_add(1);
        let next = if incremented >= Self::REPLAY_BUFFER_CAPACITY { 0 } else { incremented };
        self.expiring_nonce_ring_ptr.write(next)?;

        checkpoint.commit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, address};
    use base_precompile_storage::{
        BasePrecompileError, Handler, HashMapStorageProvider, StorageCtx, StorageKey,
    };

    use crate::{INonceManager, nonce::storage::NonceManagerStorage};

    const ACCOUNT_A: Address = address!("0x1111111111111111111111111111111111111111");
    const ACCOUNT_B: Address = address!("0x2222222222222222222222222222222222222222");

    #[test]
    fn get_nonce_returns_zero_for_new_key() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let nonce = NonceManagerStorage::new(ctx).get_nonce(ACCOUNT_A, U256::from(5)).unwrap();
            assert_eq!(nonce, 0);
        });
    }

    #[test]
    fn get_nonce_rejects_protocol_nonce() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let err = NonceManagerStorage::new(ctx).get_nonce(ACCOUNT_A, U256::ZERO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(INonceManager::ProtocolNonceNotSupported {})
            );
        });
    }

    #[test]
    fn increment_nonce_advances_and_emits_event() {
        let mut storage = HashMapStorageProvider::new(1);
        let nonce_key = U256::from(5);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            assert_eq!(mgr.increment_nonce(ACCOUNT_A, nonce_key).unwrap(), 1);
            assert_eq!(mgr.increment_nonce(ACCOUNT_A, nonce_key).unwrap(), 2);
        });
        assert_eq!(storage.get_events(NonceManagerStorage::ADDRESS).len(), 2);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_eq!(NonceManagerStorage::new(ctx).get_nonce(ACCOUNT_A, nonce_key).unwrap(), 2);
        });
    }

    #[test]
    fn loaded_increment_uses_one_sload_and_one_sstore() {
        let mut storage = HashMapStorageProvider::new(1);
        let nonce_key = U256::from(5);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);

            ctx.reset_counters();
            let current = mgr.get_nonce(ACCOUNT_A, nonce_key).unwrap();
            assert_eq!(mgr.increment_nonce_from_current(ACCOUNT_A, nonce_key, current).unwrap(), 1);
            assert_eq!(ctx.counter_sload(), 1);
            assert_eq!(ctx.counter_sstore(), 1);

            ctx.reset_counters();
            let current = mgr.get_nonce(ACCOUNT_A, nonce_key).unwrap();
            assert_eq!(mgr.increment_nonce_from_current(ACCOUNT_A, nonce_key, current).unwrap(), 2);
            assert_eq!(ctx.counter_sload(), 1);
            assert_eq!(ctx.counter_sstore(), 1);
        });
    }

    #[test]
    fn increment_nonce_rejects_protocol_nonce() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let err =
                NonceManagerStorage::new(ctx).increment_nonce(ACCOUNT_A, U256::ZERO).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(INonceManager::InvalidNonceKey {}));
        });
    }

    #[test]
    fn increment_nonce_rejects_overflow() {
        let mut storage = HashMapStorageProvider::new(1);
        let nonce_key = U256::from(9);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            // Seed the channel at the maximum so the next increment overflows.
            mgr.nonces.at_mut(&ACCOUNT_A).at_mut(&nonce_key).write(u64::MAX).unwrap();
            let err = mgr.increment_nonce(ACCOUNT_A, nonce_key).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(INonceManager::NonceOverflow {}));
        });
    }

    #[test]
    fn expiring_nonce_rejects_when_ring_slot_is_live() {
        let mut storage = HashMapStorageProvider::new(1);
        let now = 1_000u64;
        let now_ms = now * 1_000;
        storage.set_timestamp(U256::from(now));
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            // Occupy the slot the write pointer references with an unexpired entry
            // (simulating a full ring) so the slot cannot be reclaimed.
            let occupant = B256::repeat_byte(0xAB);
            let ptr = mgr.expiring_nonce_ring_ptr.read().unwrap();
            mgr.expiring_nonce_ring.at_mut(&ptr).write(occupant).unwrap();
            mgr.expiring_nonce_seen.at_mut(&occupant).write(now_ms + 20_000).unwrap();

            let err = mgr
                .check_and_mark_expiring_nonce(B256::repeat_byte(0xCD), now_ms + 20_000)
                .unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(INonceManager::ExpiringNonceSetFull {}));
        });
    }

    #[test]
    fn nonces_are_independent_per_account() {
        let mut storage = HashMapStorageProvider::new(1);
        let nonce_key = U256::from(7);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            for _ in 0..10 {
                mgr.increment_nonce(ACCOUNT_A, nonce_key).unwrap();
            }
            for _ in 0..20 {
                mgr.increment_nonce(ACCOUNT_B, nonce_key).unwrap();
            }
        });
        StorageCtx::enter(&mut storage, |ctx| {
            let mgr = NonceManagerStorage::new(ctx);
            assert_eq!(mgr.get_nonce(ACCOUNT_A, nonce_key).unwrap(), 10);
            assert_eq!(mgr.get_nonce(ACCOUNT_B, nonce_key).unwrap(), 20);
        });
    }

    #[test]
    fn expiring_nonce_rejects_replay_within_window() {
        let mut storage = HashMapStorageProvider::new(1);
        let now = 1_000u64;
        let now_ms = now * 1_000;
        storage.set_timestamp(U256::from(now));
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            let hash = B256::repeat_byte(0x11);
            mgr.check_and_mark_expiring_nonce(hash, now_ms + 20_000).unwrap();
            let err = mgr.check_and_mark_expiring_nonce(hash, now_ms + 20_000).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(INonceManager::ExpiringNonceReplay {}));
        });
    }

    #[test]
    fn expiring_nonce_validates_expiry_window() {
        let mut storage = HashMapStorageProvider::new(1);
        let now = 1_000u64;
        storage.set_timestamp(U256::from(now));
        // The inclusion-time check compares `valid_before` (ms) against
        // `block.timestamp * 1000`, so build the window relative to `now_ms`.
        let now_ms = now * 1_000;
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            let hash = B256::repeat_byte(0x22);
            let invalid = BasePrecompileError::revert(INonceManager::InvalidExpiringNonceExpiry {});

            // In the past, exactly now, and beyond the max window all fail.
            assert_eq!(mgr.check_and_mark_expiring_nonce(hash, now_ms - 1).unwrap_err(), invalid);
            assert_eq!(mgr.check_and_mark_expiring_nonce(hash, now_ms).unwrap_err(), invalid);
            assert_eq!(
                mgr.check_and_mark_expiring_nonce(
                    hash,
                    now_ms + NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW + 1
                )
                .unwrap_err(),
                invalid
            );

            // Exactly at the max window succeeds.
            mgr.check_and_mark_expiring_nonce(
                hash,
                now_ms + NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW,
            )
            .unwrap();
        });
    }

    #[test]
    fn expiring_nonce_seen_clears_after_expiry() {
        let mut storage = HashMapStorageProvider::new(1);
        let now = 1_000u64;
        let now_ms = now * 1_000;
        let valid_before = now_ms + 20_000;
        storage.set_timestamp(U256::from(now));
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            let hash = B256::repeat_byte(0x33);
            mgr.check_and_mark_expiring_nonce(hash, valid_before).unwrap();
            assert!(mgr.is_expiring_nonce_seen(hash, now_ms).unwrap());
            assert!(!mgr.is_expiring_nonce_seen(hash, valid_before + 1).unwrap());
        });
    }

    #[test]
    fn expiring_nonce_ring_pointer_wraps_at_capacity() {
        let mut storage = HashMapStorageProvider::new(1);
        let now = 1_000u64;
        let valid_before = now * 1_000 + 20_000;
        storage.set_timestamp(U256::from(now));
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            // Seed the pointer just below capacity so the next write wraps it.
            mgr.expiring_nonce_ring_ptr
                .write(NonceManagerStorage::REPLAY_BUFFER_CAPACITY - 1)
                .unwrap();

            mgr.check_and_mark_expiring_nonce(B256::repeat_byte(0x77), valid_before).unwrap();
            assert_eq!(mgr.expiring_nonce_ring_ptr.read().unwrap(), 0);

            mgr.check_and_mark_expiring_nonce(B256::repeat_byte(0x88), valid_before).unwrap();
            assert_eq!(mgr.expiring_nonce_ring_ptr.read().unwrap(), 1);
        });
    }

    #[test]
    fn nonce_slot_matches_handler_chain() {
        let mut storage = HashMapStorageProvider::new(1);
        let nonce_key = U256::from(42);
        StorageCtx::enter(&mut storage, |ctx| {
            let mgr = NonceManagerStorage::new(ctx);
            let inner_base = mgr.nonces.at(&ACCOUNT_A).slot();
            let expected = nonce_key.mapping_slot(inner_base);
            assert_eq!(NonceManagerStorage::nonce_slot(ACCOUNT_A, nonce_key).unwrap(), expected);
        });
    }

    #[test]
    fn nonce_slot_locates_value_written_via_precompile() {
        let mut storage = HashMapStorageProvider::new(1);
        let nonce_key = U256::from(7);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            for _ in 0..3 {
                mgr.increment_nonce(ACCOUNT_A, nonce_key).unwrap();
            }
        });

        let slot = NonceManagerStorage::nonce_slot(ACCOUNT_A, nonce_key).unwrap();
        StorageCtx::enter(&mut storage, |ctx| {
            let word = ctx.sload(NonceManagerStorage::ADDRESS, slot).unwrap();
            // u64 leaf is right-aligned in the slot (Solidity packing).
            let bytes = word.to_be_bytes::<32>();
            let nonce = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
            assert_eq!(nonce, 3);
        });
    }

    #[test]
    fn nonce_slot_distinct_per_account_and_key() {
        let key_a = U256::from(1);
        let key_b = U256::from(2);
        let slot_a1 = NonceManagerStorage::nonce_slot(ACCOUNT_A, key_a).unwrap();
        let slot_a2 = NonceManagerStorage::nonce_slot(ACCOUNT_A, key_b).unwrap();
        let slot_b1 = NonceManagerStorage::nonce_slot(ACCOUNT_B, key_a).unwrap();
        assert_ne!(slot_a1, slot_a2);
        assert_ne!(slot_a1, slot_b1);
        assert_ne!(slot_a2, slot_b1);
    }

    #[test]
    fn nonce_slot_rejects_protocol_nonce() {
        let err = NonceManagerStorage::nonce_slot(ACCOUNT_A, U256::ZERO).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(INonceManager::ProtocolNonceNotSupported {}));
    }

    /// Sizing invariant: the fixed replay ring buffer must be large enough that
    /// every nonce-free replay entry recorded within one expiry window has
    /// expired by the time the write pointer wraps back to its slot. If it is
    /// not, `check_and_mark_expiring_nonce` starts rejecting valid transactions
    /// with `ExpiringNonceReplay`/`ExpiringNonceSetFull` under sustained load
    /// (the pointer laps a still-live entry).
    ///
    /// Worst case: the chain is packed end-to-end with the cheapest possible
    /// nonce-free transactions for the entire window. The number of
    /// simultaneously-live entries is then
    /// `(block_gas_limit / min_tx_gas) * ceil(window / block_time)`, which must
    /// not exceed [`NonceManagerStorage::REPLAY_BUFFER_CAPACITY`].
    ///
    /// This test pins the throughput ceiling the buffer is sized for (matching
    /// the "~10k TPS for ~30s" note on `REPLAY_BUFFER_CAPACITY`). Raising
    /// [`NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW`] without growing the
    /// buffer (or shrinking the supported gas limit) breaks the invariant and
    /// fails here — a deliberate fork-level tripwire.
    #[test]
    fn replay_buffer_covers_peak_nonce_free_throughput() {
        // Conservative Base worst-case chain parameters.
        const BLOCK_GAS_LIMIT: u64 = 600_000_000;
        const BLOCK_TIME_SECS: u64 = 2;
        const MIN_TX_GAS: u64 = 30_000;

        let max_txs_per_block = BLOCK_GAS_LIMIT / MIN_TX_GAS;
        // NONCE_FREE_EXPIRY_WINDOW is in milliseconds; the block cadence is in
        // seconds, so convert the window before dividing.
        let window_secs = NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW / 1_000;
        // Round the window up to whole blocks so we never under-count.
        let blocks_per_window = window_secs.div_ceil(BLOCK_TIME_SECS);
        let max_live_entries = max_txs_per_block * blocks_per_window;

        assert!(
            u64::from(NonceManagerStorage::REPLAY_BUFFER_CAPACITY) >= max_live_entries,
            "replay buffer capacity {} cannot hold peak live entries {max_live_entries} \
             (block_gas_limit={BLOCK_GAS_LIMIT}, min_tx_gas={MIN_TX_GAS}, \
             window={window_secs}s, block_time={BLOCK_TIME_SECS}s): grow REPLAY_BUFFER_CAPACITY \
             or lower NONCE_FREE_EXPIRY_WINDOW",
            NonceManagerStorage::REPLAY_BUFFER_CAPACITY,
        );
    }
}
