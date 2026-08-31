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

use alloy_primitives::{Address, U256, keccak256};
use evm2::{
    Evm,
    registry::{HandlerError, HandlerResult},
};

use crate::BaseEvmTypes;

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
}
