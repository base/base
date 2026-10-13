//! Storage layout of the native BLS public key registry.

use alloy_primitives::{Address, B256, U256, keccak256};
use base_common_consensus::BlsConstants;

use crate::BlsState;

/// Storage layout of the native BLS public key registry at [`BlsConstants::REGISTRY_ADDRESS`].
///
/// The registry is an ordinary, code-free account. An owner's 96-byte compressed G2 public key is
/// stored as three 32-byte big-endian words in [`Self::key_slots`], so the registry is part of
/// the normal state trie and is rebuilt purely by re-executing L1-derived blocks. A registered key
/// is never all-zero (the compression bit is always set), so an all-zero key means "not
/// registered". The first registration sets the registry account nonce to
/// [`Self::ACCOUNT_NONCE`] so EIP-161 state clearing can never delete the account or its storage.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsRegistry;

impl BlsRegistry {
    /// Number of storage words holding one compressed public key.
    pub const KEY_WORDS: usize = BlsConstants::PUBKEY_LEN / 32;

    /// Nonce the registry account is given on first registration.
    pub const ACCOUNT_NONCE: u64 = 1;

    /// Returns the storage slots of `owner`'s public key:
    /// `keccak256(pad32(owner) || pad32(0)) + {0, 1, 2}`.
    pub fn key_slots(owner: Address) -> [B256; Self::KEY_WORDS] {
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(owner.into_word().as_slice());
        let base = U256::from_be_bytes(keccak256(preimage).0);
        core::array::from_fn(|index| B256::from(base.wrapping_add(U256::from(index))))
    }

    /// Splits a compressed public key into its storage words.
    pub fn key_words(public_key: &[u8; BlsConstants::PUBKEY_LEN]) -> [U256; Self::KEY_WORDS] {
        core::array::from_fn(|index| U256::from_be_slice(&public_key[index * 32..(index + 1) * 32]))
    }

    /// Joins storage words into a compressed public key, or `None` if the owner is not
    /// registered (all words zero).
    pub fn key_from_words(
        words: [U256; Self::KEY_WORDS],
    ) -> Option<[u8; BlsConstants::PUBKEY_LEN]> {
        if words.iter().all(U256::is_zero) {
            return None;
        }
        let mut public_key = [0u8; BlsConstants::PUBKEY_LEN];
        for (index, word) in words.iter().enumerate() {
            public_key[index * 32..(index + 1) * 32].copy_from_slice(&word.to_be_bytes::<32>());
        }
        Some(public_key)
    }

    /// Reads `owner`'s registered compressed public key, or `None` if it is not registered.
    ///
    /// Loads the registry account before its storage: some states, such as the proof trie
    /// database, only serve storage of accounts already loaded through `basic`.
    pub fn read_key<S: BlsState>(
        state: &mut S,
        owner: Address,
    ) -> Result<Option<[u8; BlsConstants::PUBKEY_LEN]>, S::Error> {
        state.basic(BlsConstants::REGISTRY_ADDRESS)?;
        let mut words = [U256::ZERO; Self::KEY_WORDS];
        for (word, slot) in words.iter_mut().zip(Self::key_slots(owner)) {
            *word = state.storage(BlsConstants::REGISTRY_ADDRESS, slot)?;
        }
        Ok(Self::key_from_words(words))
    }
}
