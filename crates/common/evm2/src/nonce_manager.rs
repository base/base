//! EIP-8130 2D nonce-manager storage primitive.
//!
//! The nonce manager is a code-less EIP-8130 system account (given the `0xEF` reap-protection stub
//! by the [`Cobalt`](crate::Cobalt) transition) that persists per-`(account, nonce_key)` 2D
//! sequence nonces in the state trie. This module provides the **storage layout** for those nonces
//! — the ERC-7201 slot derivation and the read path — so the eventual EIP-8130 execution layer (and
//! off-chain readers) can address channel nonces in a way that is byte-for-byte storage-compatible
//! with the revm reference (`base-common-precompiles`' `NonceManagerStorage`). The full precompile
//! (ABI dispatch, nonce increment + events, and the nonce-free replay ring buffer) is layered on
//! with the EIP-8130 track.

use alloy_primitives::{Address, B256, U256, keccak256};
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

/// EIP-8130 2D nonce-manager storage primitive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NonceManager;

impl NonceManager {
    /// The nonce-manager system account address (`NONCE_MANAGER_ADDRESS`, EIP-8130 constant table).
    pub const ADDRESS: Address =
        alloy_primitives::address!("813000000000000000000000000000000000aa01");

    /// ERC-7201 base storage slot of the `nonces` mapping under the `base.nonce_manager` namespace.
    ///
    /// Hardcoded (rather than deriving ERC-7201 here) to keep this crate revm-free; a parity test
    /// pins it to the reference `NonceManagerStorage::NONCES_BASE_SLOT`.
    pub const NONCES_BASE_SLOT: U256 = alloy_primitives::uint!(
        0x9d3ea32ad25774a46482ebbae019e8da4242109d164d324a59aa787515b9fe00_U256
    );

    /// ERC-7201 base storage slot of the nonce-free replay `expiring_nonce_seen` mapping.
    ///
    /// Hardcoded to keep this crate revm-free; a parity test pins it to the reference
    /// `NonceManagerStorage::EXPIRING_NONCE_SEEN_BASE_SLOT`.
    pub const EXPIRING_NONCE_SEEN_BASE_SLOT: U256 = alloy_primitives::uint!(
        0x9d3ea32ad25774a46482ebbae019e8da4242109d164d324a59aa787515b9fe01_U256
    );

    /// The reserved protocol nonce key (held in account state, not managed here).
    pub const PROTOCOL_NONCE_KEY: U256 = U256::ZERO;

    /// Returns the storage slot holding the 2D channel nonce for `nonces[account][nonce_key]`, or
    /// `None` for the reserved protocol nonce key (`0`), which lives in account state.
    ///
    /// Mirrors the reference `NonceManagerStorage::nonce_slot`: two nested Solidity mappings,
    /// `nonce_key => (account => base)`, each slot `keccak256(pad32(key) ++ be32(slot))`.
    pub fn nonce_slot(account: Address, nonce_key: U256) -> Option<U256> {
        if nonce_key == Self::PROTOCOL_NONCE_KEY {
            return None;
        }
        let inner = Self::address_mapping_slot(account, Self::NONCES_BASE_SLOT);
        Some(Self::u256_mapping_slot(nonce_key, inner))
    }

    /// Returns the current 2D nonce for `account` at `nonce_key`, or `None` for the reserved
    /// protocol nonce key (`0`). Reads storage untracked, so it does not perturb execution.
    pub fn get_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        account: Address,
        nonce_key: U256,
    ) -> HandlerResult<Option<u64>> {
        let Some(slot) = Self::nonce_slot(account, nonce_key) else {
            return Ok(None);
        };
        let value = evm
            .state_mut()
            .storage_slot_untracked(&Self::ADDRESS, &slot)
            .map_err(HandlerError::Fatal)?;
        Ok(Some(value.saturating_to::<u64>()))
    }

    /// Returns the storage slot holding the recorded expiry for a nonce-free transaction's
    /// `replay_id`. Mirrors the reference `NonceManagerStorage::expiring_nonce_seen_slot`.
    pub fn expiring_nonce_seen_slot(replay_id: B256) -> U256 {
        Self::u256_mapping_slot(
            U256::from_be_bytes(replay_id.0),
            Self::EXPIRING_NONCE_SEEN_BASE_SLOT,
        )
    }

    /// Returns whether `replay_id` has been recorded and has not yet expired relative to `now`
    /// (Unix milliseconds) — the nonce-free replay check. Reads storage untracked.
    pub fn is_expiring_nonce_seen(
        evm: &mut Evm<'_, BaseEvmTypes>,
        replay_id: B256,
        now: u64,
    ) -> HandlerResult<bool> {
        let slot = Self::expiring_nonce_seen_slot(replay_id);
        let expiry = evm
            .state_mut()
            .storage_slot_untracked(&Self::ADDRESS, &slot)
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
        let Some(slot) = Self::nonce_slot(account, nonce_key) else {
            return Ok(None);
        };
        let mut handle = evm
            .state_mut()
            .storage_slot(&Self::ADDRESS, slot, false)
            .map_err(HandlerError::Fatal)?;
        let current = handle.current().saturating_to::<u64>();
        let new_nonce =
            current.checked_add(1).ok_or_else(|| HandlerError::external(NonceOverflow))?;
        handle.set(U256::from(new_nonce));
        Ok(Some(new_nonce))
    }

    /// The Solidity slot for `mapping[address_key]` at base `slot`: `keccak256(pad32(key) ++ slot)`.
    fn address_mapping_slot(key: Address, slot: U256) -> U256 {
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(key.as_slice());
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }

    /// The Solidity slot for `mapping[u256_key]` at base `slot`: `keccak256(be32(key) ++ slot)`.
    fn u256_mapping_slot(key: U256, slot: U256) -> U256 {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&key.to_be_bytes::<32>());
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
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
        let spec = BaseSpecId::new(BaseUpgrade::Cobalt);
        Evm::new(
            spec,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(),
            db,
            Precompiles::base(spec.into()),
        )
    }

    #[test]
    fn protocol_nonce_key_has_no_channel_slot() {
        assert_eq!(NonceManager::nonce_slot(ACCOUNT, NonceManager::PROTOCOL_NONCE_KEY), None);
    }

    #[test]
    fn address_and_base_slot_match_the_revm_reference() {
        use base_common_precompiles::NonceManagerStorage;
        assert_eq!(NonceManager::ADDRESS, NonceManagerStorage::ADDRESS);
        assert_eq!(NonceManager::NONCES_BASE_SLOT, NonceManagerStorage::NONCES_BASE_SLOT);
    }

    #[test]
    fn nonce_slot_matches_the_revm_reference() {
        use base_common_precompiles::NonceManagerStorage;
        // Sweep several accounts and channel keys; the evm2 layout must match the reference slot.
        for account in [ACCOUNT, address!("0x1111111111111111111111111111111111111111")] {
            for key in [U256::from(1u64), KEY, U256::MAX] {
                assert_eq!(
                    NonceManager::nonce_slot(account, key),
                    NonceManagerStorage::nonce_slot(account, key).ok(),
                    "nonce slot diverged for {account} key {key}",
                );
            }
        }
    }

    #[test]
    fn expiring_nonce_seen_slot_matches_the_revm_reference() {
        use base_common_precompiles::NonceManagerStorage;
        assert_eq!(
            NonceManager::EXPIRING_NONCE_SEEN_BASE_SLOT,
            NonceManagerStorage::EXPIRING_NONCE_SEEN_BASE_SLOT,
        );
        for id in [B256::repeat_byte(0x11), B256::repeat_byte(0xff), B256::ZERO] {
            assert_eq!(
                NonceManager::expiring_nonce_seen_slot(id),
                NonceManagerStorage::expiring_nonce_seen_slot(id),
                "expiring-nonce slot diverged for {id}",
            );
        }
    }

    #[test]
    fn is_expiring_nonce_seen_respects_recorded_expiry() {
        let replay_id = B256::repeat_byte(0x42);
        let slot = NonceManager::expiring_nonce_seen_slot(replay_id);
        let mut db = InMemoryDB::default();
        // Recorded expiry of 5_000 ms.
        db.insert_account_storage(&NonceManager::ADDRESS, &slot, &U256::from(5_000u64));
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
        let slot = NonceManager::nonce_slot(ACCOUNT, KEY).expect("channel key has a slot");
        let mut db = InMemoryDB::default();
        db.insert_account_storage(&NonceManager::ADDRESS, &slot, &U256::from(42u64));
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
            NonceManager::get_nonce(&mut evm, ACCOUNT, NonceManager::PROTOCOL_NONCE_KEY).unwrap(),
            None,
        );
    }

    #[test]
    fn increment_nonce_advances_the_channel() {
        let slot = NonceManager::nonce_slot(ACCOUNT, KEY).expect("channel key has a slot");
        let mut evm = evm(InMemoryDB::default());
        // Two increments from zero advance the channel to 1 then 2, each reflected in storage.
        assert_eq!(NonceManager::increment_nonce(&mut evm, ACCOUNT, KEY).unwrap(), Some(1));
        assert_eq!(NonceManager::increment_nonce(&mut evm, ACCOUNT, KEY).unwrap(), Some(2));
        let stored =
            evm.state_mut().storage_slot(&NonceManager::ADDRESS, slot, false).unwrap().current();
        assert_eq!(stored, U256::from(2u64));
    }

    #[test]
    fn increment_nonce_returns_none_for_protocol_key() {
        let mut evm = evm(InMemoryDB::default());
        assert_eq!(
            NonceManager::increment_nonce(&mut evm, ACCOUNT, NonceManager::PROTOCOL_NONCE_KEY)
                .unwrap(),
            None,
        );
    }
}
