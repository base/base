//! Base transaction 2D nonce-manager storage access for the EVM2 engine.
//!
//! The nonce manager is a code-less Base transaction system account (given the `0xEF`
//! reap-protection stub by the [`Everest`](crate::Everest) transition) that persists per-`(account,
//! nonce_key)` 2D sequence nonces in the state trie. The engine-neutral slot layout — the
//! system-account address, the ERC-7201 base slots, and the mapping-slot derivation — lives in
//! [`base_common_base_tx::NonceManagerSlots`], shared with the revm path so the two engines cannot
//! diverge. This module provides the EVM2-specific access layer over that layout: the untracked
//! reads, and the tracked reads and writes the Base transaction executor makes: the channel-nonce
//! increment and the nonce-free replay ring buffer. Tracked accesses warm the slots they touch, as
//! the reference's journal reads do, so a later `getNonce` call in the same transaction is charged
//! the warm rate. The ABI surface (`getNonce`) is the nonce manager precompile's.
//!
//! The storage layout is the reference contract's (`base_common_precompiles::NonceManagerStorage`):
//!
//! ```solidity
//! mapping(address => mapping(uint256 => uint64)) nonces;  // root + 0
//! mapping(bytes32 => uint64) expiringNonceSeen;           // root + 1: replay id => expiry (ms)
//! mapping(uint32 => bytes32) expiringNonceRing;           // root + 2: circular buffer
//! uint32 expiringNonceRingPtr;                            // root + 3: ring write position
//! ```
//!
//! Every value sits at byte offset 0 of its own slot (the low bytes of the word).

use alloy_primitives::{Address, B256, Bytes, U256};
use base_common_base_tx::NonceManagerSlots;
use base_tiger_precompiles::NonceManagerStorage;
use evm2::{
    Evm,
    bytecode::Bytecode,
    registry::{HandlerError, HandlerResult},
};

use crate::BaseEvmTypes;

/// Error returned when a 2D channel nonce would overflow `u64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonceOverflow;

impl core::fmt::Display for NonceOverflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Base transaction 2D channel nonce overflowed u64")
    }
}

impl core::error::Error for NonceOverflow {}

/// Error returned when a nonce-free (`NONCE_KEY_MAX`) transaction cannot be recorded in the replay
/// ring buffer. Mirrors the reference `INonceManager` reverts of `check_and_mark_expiring_nonce`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiringNonceError {
    /// The replay id is zero, the ring's empty-slot sentinel.
    InvalidReplayId,
    /// `valid_before` is not in `(now_ms, now_ms + NONCE_FREE_EXPIRY_WINDOW]`.
    InvalidExpiry,
    /// The replay id is already recorded and unexpired.
    Replay,
    /// The ring slot the write pointer reaches holds an unexpired entry.
    SetFull,
}

impl core::fmt::Display for ExpiringNonceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InvalidReplayId => "nonce-free replay id is zero",
            Self::InvalidExpiry => "nonce-free valid_before is outside the expiry window",
            Self::Replay => "nonce-free replay hash already recorded",
            Self::SetFull => "nonce-free replay set is full",
        })
    }
}

impl core::error::Error for ExpiringNonceError {}

/// EVM2 access layer for the Base transaction 2D nonce-manager storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NonceManager;

impl NonceManager {
    /// Code the reference writes to the nonce manager account before any persistent write (the
    /// precompile storage `__initialize`), the same `0xEF` stub the Everest transition plants.
    const ACCOUNT_STUB: [u8; 1] = [0xEF];

    /// The `uint64` stored at offset 0 of a slot word.
    const fn low_u64(word: U256) -> u64 {
        word.as_limbs()[0]
    }

    /// Reads a nonce manager slot through the transaction overlay, warming it.
    fn load(evm: &mut Evm<'_, BaseEvmTypes>, slot: U256) -> HandlerResult<U256> {
        let mut handle = evm
            .state_mut()
            .storage_slot(&NonceManagerSlots::ADDRESS, slot, false)
            .map_err(HandlerError::Fatal)?;
        let _ = handle.warm();
        Ok(handle.current())
    }

    /// Writes the low `width_bytes` of a nonce manager slot, preserving the rest of the word (the
    /// packed write of the reference storage layer), and warms the slot.
    fn store(
        evm: &mut Evm<'_, BaseEvmTypes>,
        slot: U256,
        width_bytes: usize,
        value: U256,
    ) -> HandlerResult<()> {
        assert!(width_bytes > 0 && width_bytes <= 32, "packed field width out of range");
        let mask = if width_bytes == 32 {
            U256::MAX
        } else {
            (U256::ONE << (width_bytes * 8)) - U256::ONE
        };
        assert!(value <= mask, "packed field value wider than its slot field");
        let mut handle = evm
            .state_mut()
            .storage_slot(&NonceManagerSlots::ADDRESS, slot, false)
            .map_err(HandlerError::Fatal)?;
        let _ = handle.warm();
        let word = (handle.current() & !mask) | value;
        handle.set(word);
        Ok(())
    }

    /// Gives the nonce manager account its `0xEF` code stub unless it already carries it, the
    /// reference's `__initialize` before a persistent write. The Everest transition has normally
    /// planted it already, making this a no-op.
    fn ensure_stub(evm: &mut Evm<'_, BaseEvmTypes>) -> HandlerResult<()> {
        let stub = Bytecode::new_legacy(Bytes::from_static(&Self::ACCOUNT_STUB));
        let stub_hash = stub.hash_slow();
        let mut account = evm
            .state_mut()
            .account(&NonceManagerSlots::ADDRESS, false)
            .map_err(HandlerError::Fatal)?;
        if account.code_hash() != stub_hash {
            account.set_code(stub_hash, stub);
        }
        Ok(())
    }

    /// Returns the 2D channel nonce for `account` at a non-protocol `nonce_key`, read through the
    /// transaction overlay so the slot becomes warm.
    pub fn channel_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        account: Address,
        nonce_key: U256,
    ) -> HandlerResult<u64> {
        let slot = NonceManagerSlots::nonce_slot(account, nonce_key)
            .expect("the protocol nonce key has no channel slot");
        Ok(Self::low_u64(Self::load(evm, slot)?))
    }

    /// Returns whether `replay_id` is recorded and unexpired at `now_ms`, read through the
    /// transaction overlay so the slot becomes warm (the executor's nonce check).
    pub fn expiring_nonce_seen(
        evm: &mut Evm<'_, BaseEvmTypes>,
        replay_id: B256,
        now_ms: u64,
    ) -> HandlerResult<bool> {
        let slot = NonceManagerSlots::expiring_nonce_seen_slot(replay_id);
        let expiry = Self::low_u64(Self::load(evm, slot)?);
        Ok(expiry != 0 && expiry > now_ms)
    }

    /// Validates and records a nonce-free transaction's replay id with expiry `valid_before_ms`,
    /// reclaiming the ring slot the write pointer reaches. Mirrors the reference
    /// `check_and_mark_expiring_nonce`; the caller discards the transaction on error, so a failed
    /// check leaves no writes behind.
    pub fn check_and_mark_expiring_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        replay_id: B256,
        valid_before_ms: u64,
        now_ms: u64,
    ) -> HandlerResult<()> {
        if replay_id == B256::ZERO {
            return Err(HandlerError::external(ExpiringNonceError::InvalidReplayId));
        }
        if valid_before_ms <= now_ms
            || valid_before_ms > now_ms.saturating_add(NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW)
        {
            return Err(HandlerError::external(ExpiringNonceError::InvalidExpiry));
        }
        let seen_slot = NonceManagerSlots::expiring_nonce_seen_slot(replay_id);
        let seen_expiry = Self::low_u64(Self::load(evm, seen_slot)?);
        if seen_expiry != 0 && seen_expiry > now_ms {
            return Err(HandlerError::external(ExpiringNonceError::Replay));
        }

        Self::ensure_stub(evm)?;

        // A pointer at or past capacity can only come from corrupted state; restart at 0.
        let ptr_word = Self::load(evm, NonceManagerStorage::EXPIRING_NONCE_RING_PTR)?;
        let ptr = match u32::try_from(ptr_word.as_limbs()[0] & u64::from(u32::MAX)) {
            Ok(ptr) if ptr < NonceManagerStorage::REPLAY_BUFFER_CAPACITY => ptr,
            _ => 0,
        };
        let ring_slot = NonceManagerStorage::ring_slot(ptr);
        let old_hash = B256::from(Self::load(evm, ring_slot)?.to_be_bytes::<32>());
        if old_hash != B256::ZERO {
            let old_seen_slot = NonceManagerSlots::expiring_nonce_seen_slot(old_hash);
            let old_expiry = Self::low_u64(Self::load(evm, old_seen_slot)?);
            if old_expiry != 0 && old_expiry > now_ms {
                return Err(HandlerError::external(ExpiringNonceError::SetFull));
            }
            Self::store(evm, old_seen_slot, 8, U256::ZERO)?;
        }

        Self::store(evm, ring_slot, 32, U256::from_be_bytes(replay_id.0))?;
        Self::store(evm, seen_slot, 8, U256::from(valid_before_ms))?;
        let next = (ptr + 1) % NonceManagerStorage::REPLAY_BUFFER_CAPACITY;
        Self::store(evm, NonceManagerStorage::EXPIRING_NONCE_RING_PTR, 4, U256::from(next))?;
        Ok(())
    }

    /// Increments the 2D channel nonce for `account` at `nonce_key`, returning the new value, or
    /// `None` for the reserved protocol nonce key (`0`). Writes to the transaction state overlay
    /// (the increment commits with the transaction), warming the slot and planting the account
    /// stub first, mirroring the reference's in-execution write. The caller must not pass
    /// `NONCE_KEY_MAX` (nonce-free mode has no channel).
    pub fn increment_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        account: Address,
        nonce_key: U256,
    ) -> HandlerResult<Option<u64>> {
        let Some(slot) = NonceManagerSlots::nonce_slot(account, nonce_key) else {
            return Ok(None);
        };
        let current = Self::low_u64(Self::load(evm, slot)?);
        let new_nonce =
            current.checked_add(1).ok_or_else(|| HandlerError::external(NonceOverflow))?;
        Self::ensure_stub(evm)?;
        Self::store(evm, slot, 8, U256::from(new_nonce))?;
        Ok(Some(new_nonce))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use base_common_genesis::BaseUpgrade;
    use evm2::{Precompiles, env::BlockEnv, evm::InMemoryDB};

    use super::*;
    use crate::BaseSpecId;

    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000aa");
    const KEY: U256 = U256::from_limbs([7, 0, 0, 0]);

    fn evm(db: InMemoryDB) -> Evm<'static, BaseEvmTypes> {
        let spec = BaseSpecId::new(BaseUpgrade::Everest);
        Evm::new(
            spec,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(spec),
            db,
            Precompiles::base(spec.into()),
        )
    }

    #[test]
    fn increment_nonce_advances_the_channel() {
        let slot = NonceManagerSlots::nonce_slot(ACCOUNT, KEY).expect("channel key has a slot");
        let mut evm = evm(InMemoryDB::default());
        // Two increments from zero advance the channel to 1 then 2, each reflected in storage.
        assert_eq!(NonceManager::increment_nonce(&mut evm, ACCOUNT, KEY).unwrap(), Some(1));
        assert_eq!(NonceManager::increment_nonce(&mut evm, ACCOUNT, KEY).unwrap(), Some(2));
        let stored = evm
            .state_mut()
            .storage_slot(&NonceManagerSlots::ADDRESS, slot, false)
            .unwrap()
            .current();
        assert_eq!(stored, U256::from(2u64));
    }

    #[test]
    fn increment_nonce_returns_none_for_protocol_key() {
        let mut evm = evm(InMemoryDB::default());
        assert_eq!(
            NonceManager::increment_nonce(&mut evm, ACCOUNT, NonceManagerSlots::PROTOCOL_NONCE_KEY)
                .unwrap(),
            None,
        );
    }
}
