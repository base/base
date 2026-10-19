//! Fixtures shared by the unit tests of this crate: deterministic secp256k1
//! keys and signatures, canonical storage-word packing, a fresh in-memory
//! account-configuration store, and a baseline [`TxEip8130`].

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_common_consensus::{AccountChange, Eip8130Constants, TxEip8130};
use base_precompile_storage::{HashMapStorageProvider, StorageCtx};
use k256::ecdsa::SigningKey as K256SigningKey;

use crate::AccountConfigurationStorage;

/// Wall-clock seconds used as `now` by tests.
pub const NOW: u64 = 1_000;

/// Chain id used by tests that bind the local channel.
pub const LOCAL: u64 = 8453;

/// The native secp256k1 authenticator address.
pub const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;

/// A deterministic secp256k1 key whose 32 bytes all equal `byte`.
pub fn key(byte: u8) -> K256SigningKey {
    K256SigningKey::from_slice(&[byte; 32]).unwrap()
}

/// The Ethereum address of `key`.
pub fn addr(key: &K256SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

/// The self actor id of `account` (the address right-aligned in a word).
pub fn actor_id(account: Address) -> B256 {
    AccountConfigurationStorage::self_actor_id(account)
}

/// 65-byte `r || s || v` signature over `hash`, `v` in `{27, 28}`, low-s.
pub fn sig(key: &K256SigningKey, hash: B256) -> Vec<u8> {
    let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
    let mut out = vec![0u8; 65];
    out[..64].copy_from_slice(&signature.to_bytes());
    out[64] = recid.to_byte() + 27;
    out
}

/// `authenticator(20) || data`.
pub fn auth_blob(authenticator: Address, data: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(20 + data.len());
    out.extend_from_slice(authenticator.as_slice());
    out.extend_from_slice(data);
    Bytes::from(out)
}

/// Canonical Solidity packing of `ActorConfig` (authenticator 0..160, expiry
/// 160..208, scope 208..224). Independent of the production decoder, so
/// agreement cross-checks the layout.
pub fn pack(authenticator: Address, scope: u16, expiry: u64) -> U256 {
    U256::from_be_slice(authenticator.as_slice())
        | (U256::from(expiry) << 160)
        | (U256::from(scope) << 208)
}

/// Canonical Solidity packing of `AccountState` (`local` is the low-uint32
/// local sequence, local epoch left zero).
pub fn pack_state(multichain: u64, local: u64, flags: u8, lock_union: u64) -> U256 {
    pack_state_epoch(multichain, local, 0, flags, lock_union)
}

/// [`pack_state`] with an explicit local epoch (uint32 at bytes `16..20`).
pub fn pack_state_epoch(
    multichain: u64,
    local: u64,
    epoch: u64,
    flags: u8,
    lock_union: u64,
) -> U256 {
    let mut b = [0u8; 32];
    b[24..32].copy_from_slice(&multichain.to_be_bytes());
    b[20..24].copy_from_slice(&local.to_be_bytes()[4..]); // uint32 localSequence
    b[16..20].copy_from_slice(&epoch.to_be_bytes()[4..]); // uint32 localEpoch
    b[15] = flags;
    b[9..15].copy_from_slice(&lock_union.to_be_bytes()[2..]); // uint48 lockUnion
    U256::from_be_bytes(b)
}

/// Packs the inline secp256k1 self actor's `default_eoa_scope` (bits 232..248).
pub fn pack_default_eoa_scope(scope: u16) -> U256 {
    U256::from(scope) << 232
}

/// Runs `body` with a fresh in-memory [`AccountConfigurationStorage`].
pub fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
    let mut storage = HashMapStorageProvider::new(1);
    StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
}

/// A baseline transaction on [`LOCAL`] with no calls and the given `sender`,
/// `payer` and `account_changes`.
pub fn tx_with(
    sender: Option<Address>,
    payer: Option<Address>,
    account_changes: Vec<AccountChange>,
) -> TxEip8130 {
    TxEip8130 {
        chain_id: LOCAL,
        sender,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        valid_after: 0,
        valid_before: 0,
        max_priority_fee_per_gas: 1_000_000_000,
        max_fee_per_gas: 5_000_000_000,
        gas_limit: 250_000,
        account_changes,
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer,
    }
}
