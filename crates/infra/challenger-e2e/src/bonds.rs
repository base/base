//! Storage patches that let the bonds phase run in minutes.
//!
//! A challenged game finalizes `SLOW_FINALIZATION_DELAY` (5 days) after the
//! challenge, and its bond reaches the recipient a `DelayedWETH` delay (1 day
//! on zeronet) after the unlock. Warping L1 time would satisfy the contracts
//! but not the challenger: it compares the `DelayedWETH` unlock timestamp, a
//! block timestamp, against its own wall clock, so on a warped fork it would
//! wait out the days for real. The phase instead moves the deadlines back to
//! now, which both the contracts and the challenger accept.
//!
//! Every patch is read back through the contract's own getter before the run
//! relies on it, so a storage layout that drifts from these slots fails the
//! run on the patch, not as a challenger that never acted.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use base_proof_contracts::GameStatus;
use eyre::{Context, Result, bail, ensure};

/// The storage slots and patches the bonds phase relies on.
#[derive(Debug, Clone, Copy)]
pub struct BondStorage;

impl BondStorage {
    /// `AggregateVerifier` slot 0 packs `createdAt` (bytes 0-7 from the right),
    /// `resolvedAt` (8-15) and `status` (16).
    pub const STATUS_SLOT: U256 = U256::ZERO;

    /// `AggregateVerifier` slot 7 packs `expectedResolution` (bytes 0-7 from the
    /// right) and `proofCount` (8).
    pub const EXPECTED_RESOLUTION_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

    /// Highest `DelayedWETH` storage slot searched for the `withdrawals` mapping.
    pub const MAX_WITHDRAWALS_BASE_SLOT: u64 = 64;

    /// Byte index of `status` in the big-endian slot-0 word.
    pub const STATUS_BYTE: usize = 31 - 16;

    /// `word` with the `status` byte set to `status`; every other field kept.
    pub fn with_status(word: B256, status: GameStatus) -> B256 {
        let mut word = word;
        word[Self::STATUS_BYTE] = status as u8;
        word
    }

    /// `word` with `expectedResolution` set to `timestamp`; every other field kept.
    pub fn with_expected_resolution(word: B256, timestamp: u64) -> B256 {
        let mut word = word;
        word[24..].copy_from_slice(&timestamp.to_be_bytes());
        word
    }

    /// Slot of `withdrawals[owner][guy].amount` if the mapping sits at `base`; the
    /// request's `timestamp` is the next slot.
    pub fn withdrawal_slot(base: u64, owner: Address, guy: Address) -> U256 {
        let outer = keccak256([owner.into_word().0, U256::from(base).to_be_bytes::<32>()].concat());
        U256::from_be_bytes(keccak256([guy.into_word().0, outer.0].concat()).0)
    }

    /// Finds the slot of `withdrawals[owner][guy].amount` on the `DelayedWETH` at
    /// `weth`, given the request's current `timestamp`.
    ///
    /// Searched rather than hardcoded: the mapping's base slot depends on every
    /// contract `DelayedWETH` inherits from. A nonzero timestamp next to a
    /// nonzero amount at a candidate slot cannot occur by chance.
    pub async fn find_withdrawal_slot(
        provider: &RootProvider,
        weth: Address,
        owner: Address,
        guy: Address,
        timestamp: u64,
    ) -> Result<U256> {
        for base in 0..=Self::MAX_WITHDRAWALS_BASE_SLOT {
            let slot = Self::withdrawal_slot(base, owner, guy);
            if provider.get_storage_at(weth, slot + U256::from(1)).await? == U256::from(timestamp)
                && !provider.get_storage_at(weth, slot).await?.is_zero()
            {
                return Ok(slot);
            }
        }
        bail!(
            "found no withdrawals[{owner}][{guy}] request with timestamp {timestamp} on DelayedWETH \
             {weth} in base slots 0..={}",
            Self::MAX_WITHDRAWALS_BASE_SLOT
        )
    }

    /// Reads one storage word.
    pub async fn storage(provider: &RootProvider, address: Address, slot: U256) -> Result<B256> {
        Ok(provider.get_storage_at(address, slot).await?.into())
    }

    /// Writes one storage word on the fork.
    pub async fn set_storage(
        provider: &RootProvider,
        address: Address,
        slot: U256,
        value: B256,
    ) -> Result<()> {
        let updated = provider
            .client()
            .request::<_, bool>("anvil_setStorageAt", (address, B256::from(slot), value))
            .await
            .with_context(|| format!("anvil_setStorageAt failed for {address}"))?;
        ensure!(updated, "anvil_setStorageAt returned false for {address}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};

    use super::*;

    /// Slot 0 of a zeronet game: created, unresolved, in progress, initialized,
    /// respected when created.
    const SLOT0: B256 = b256!("0x000000000000000000000000000101000000000000000000000000006acf9fec");

    /// Slot 7 of the same game: `expectedResolution` 1792423788, one proof.
    const SLOT7: B256 = b256!("0x000000000000000000000000000000000000000000000001000000006ad6376c");

    #[test]
    fn status_patch_touches_only_the_status_byte() {
        let patched = BondStorage::with_status(SLOT0, GameStatus::DefenderWins);
        assert_eq!(
            patched,
            b256!("0x000000000000000000000000000101020000000000000000000000006acf9fec")
        );
        assert_eq!(BondStorage::with_status(patched, GameStatus::InProgress), SLOT0);
    }

    #[test]
    fn expected_resolution_patch_keeps_the_proof_count() {
        assert_eq!(
            BondStorage::with_expected_resolution(SLOT7, 0x1234),
            b256!("0x0000000000000000000000000000000000000000000000010000000000001234")
        );
    }

    #[test]
    fn withdrawal_slot_is_the_nested_mapping_slot() {
        // keccak256(guy . keccak256(owner . base)), Solidity's nested-mapping rule.
        let owner = address!("0x1111111111111111111111111111111111111111");
        let guy = address!("0x2222222222222222222222222222222222222222");
        let mut outer = [0u8; 64];
        outer[12..32].copy_from_slice(owner.as_slice());
        outer[63] = 3;
        let mut inner = [0u8; 64];
        inner[12..32].copy_from_slice(guy.as_slice());
        inner[32..].copy_from_slice(keccak256(outer).as_slice());
        assert_eq!(
            BondStorage::withdrawal_slot(3, owner, guy),
            U256::from_be_bytes(keccak256(inner).0)
        );
    }
}
