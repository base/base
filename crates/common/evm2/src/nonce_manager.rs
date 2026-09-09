//! EIP-8130 2D nonce-manager storage access for the EVM2 engine.
//!
//! The nonce manager is a code-less EIP-8130 system account (given the `0xEF` reap-protection stub
//! by the [`Zenith`](crate::Zenith) transition) that persists per-`(account, nonce_key)` 2D
//! sequence nonces in the state trie. The engine-neutral slot layout — the system-account address,
//! the ERC-7201 base slots, and the mapping-slot derivation — lives in
//! [`base_common_eip8130::NonceManagerSlots`], shared with the revm path so the two engines cannot
//! diverge. This module provides the EVM2-specific access layer over that layout: the untracked
//! reads and the channel-nonce increment against an [`Evm`] state. The remaining precompile surface
//! (ABI dispatch, the `NonceIncremented` events, and the nonce-free replay ring buffer) is layered
//! on with the EIP-8130 track.

use alloy_primitives::{Address, B256, U256};
use base_common_eip8130::NonceManagerSlots;
use evm2::{
    Evm,
    registry::{HandlerError, HandlerResult},
};

use crate::BaseEvmTypes;

/// Error returned when a 2D channel nonce would overflow `u64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonceOverflow;

impl core::fmt::Display for NonceOverflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EIP-8130 2D channel nonce overflowed u64")
    }
}

impl core::error::Error for NonceOverflow {}

/// EVM2 access layer for the EIP-8130 2D nonce-manager storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NonceManager;

impl NonceManager {
    /// Returns the current 2D nonce for `account` at `nonce_key`, or `None` for the reserved
    /// protocol nonce key (`0`). Reads storage untracked, so it does not perturb execution.
    pub fn get_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        account: Address,
        nonce_key: U256,
    ) -> HandlerResult<Option<u64>> {
        let Some(slot) = NonceManagerSlots::nonce_slot(account, nonce_key) else {
            return Ok(None);
        };
        let value = evm
            .state_mut()
            .storage_slot_untracked(&NonceManagerSlots::ADDRESS, &slot)
            .map_err(HandlerError::Fatal)?;
        Ok(Some(value.saturating_to::<u64>()))
    }

    /// Returns whether `replay_id` has been recorded and has not yet expired relative to `now`
    /// (Unix milliseconds) — the nonce-free replay check. Reads storage untracked.
    pub fn is_expiring_nonce_seen(
        evm: &mut Evm<'_, BaseEvmTypes>,
        replay_id: B256,
        now: u64,
    ) -> HandlerResult<bool> {
        let slot = NonceManagerSlots::expiring_nonce_seen_slot(replay_id);
        let expiry = evm
            .state_mut()
            .storage_slot_untracked(&NonceManagerSlots::ADDRESS, &slot)
            .map_err(HandlerError::Fatal)?
            .saturating_to::<u64>();
        Ok(expiry != 0 && expiry > now)
    }

    /// Increments the 2D channel nonce for `account` at `nonce_key`, returning the new value, or
    /// `None` for the reserved protocol nonce key (`0`). Writes to the transaction state overlay
    /// (the increment commits with the transaction), mirroring the reference's in-execution write.
    pub fn increment_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        account: Address,
        nonce_key: U256,
    ) -> HandlerResult<Option<u64>> {
        let Some(slot) = NonceManagerSlots::nonce_slot(account, nonce_key) else {
            return Ok(None);
        };
        let mut handle = evm
            .state_mut()
            .storage_slot(&NonceManagerSlots::ADDRESS, slot, false)
            .map_err(HandlerError::Fatal)?;
        let current = handle.current().saturating_to::<u64>();
        let new_nonce =
            current.checked_add(1).ok_or_else(|| HandlerError::external(NonceOverflow))?;
        handle.set(U256::from(new_nonce));
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
        let spec = BaseSpecId::new(BaseUpgrade::Zenith);
        Evm::new(
            spec,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(),
            db,
            Precompiles::base(spec.into()),
        )
    }

    #[test]
    fn is_expiring_nonce_seen_respects_recorded_expiry() {
        let replay_id = B256::repeat_byte(0x42);
        let slot = NonceManagerSlots::expiring_nonce_seen_slot(replay_id);
        let mut db = InMemoryDB::default();
        // Recorded expiry of 5_000 ms.
        db.insert_account_storage(&NonceManagerSlots::ADDRESS, &slot, &U256::from(5_000u64));
        let mut evm = evm(db);
        // Seen while now < expiry; not seen once now >= expiry (the entry has lapsed).
        assert!(NonceManager::is_expiring_nonce_seen(&mut evm, replay_id, 4_999).unwrap());
        assert!(!NonceManager::is_expiring_nonce_seen(&mut evm, replay_id, 5_000).unwrap());
        // An unrecorded id is never seen.
        assert!(
            !NonceManager::is_expiring_nonce_seen(&mut evm, B256::repeat_byte(0x99), 0).unwrap()
        );
    }

    #[test]
    fn get_nonce_reads_the_channel_slot() {
        let slot = NonceManagerSlots::nonce_slot(ACCOUNT, KEY).expect("channel key has a slot");
        let mut db = InMemoryDB::default();
        db.insert_account_storage(&NonceManagerSlots::ADDRESS, &slot, &U256::from(42u64));
        let mut evm = evm(db);
        assert_eq!(NonceManager::get_nonce(&mut evm, ACCOUNT, KEY).unwrap(), Some(42));
    }

    #[test]
    fn get_nonce_is_zero_when_unset() {
        let mut evm = evm(InMemoryDB::default());
        assert_eq!(NonceManager::get_nonce(&mut evm, ACCOUNT, KEY).unwrap(), Some(0));
    }

    #[test]
    fn get_nonce_returns_none_for_protocol_key() {
        let mut evm = evm(InMemoryDB::default());
        assert_eq!(
            NonceManager::get_nonce(&mut evm, ACCOUNT, NonceManagerSlots::PROTOCOL_NONCE_KEY)
                .unwrap(),
            None,
        );
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
