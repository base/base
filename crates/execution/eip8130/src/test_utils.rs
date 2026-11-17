//! Fixtures shared by the unit tests of this crate: deterministic secp256k1
//! keys and signatures and a baseline [`TxEip8130`].

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_common_consensus::{AccountChange, Eip8130Constants, TxEip8130};
use k256::ecdsa::SigningKey as K256SigningKey;

/// Chain id of the baseline transaction.
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
