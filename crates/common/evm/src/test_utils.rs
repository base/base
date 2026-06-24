//! Shared test helpers for constructing and signing EIP-8130 transactions,
//! used by the EIP-8130 executor and block-executor test modules so the
//! signing/address derivation and the canonical transaction shape live in a
//! single place rather than being duplicated across test modules.

use alloc::{vec, vec::Vec};

use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use base_common_consensus::TxEip8130;
use k256::ecdsa::SigningKey;

/// Base chain id used by the EIP-8130 tests.
pub const CHAIN_ID: u64 = 8453;
/// Block base fee per gas used by the EIP-8130 tests.
pub const BASE_FEE: u64 = 1_000_000_000;
/// Block beneficiary used by the EIP-8130 tests.
pub const BENEFICIARY: Address = address!("0x00000000000000000000000000000000000000bb");

/// Returns a deterministic k256 signing key derived from a single repeated byte.
pub fn signing_key(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}

/// Derives the EOA address controlled by a k256 signing key.
pub fn eoa_address(key: &SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

/// 65-byte `r || s || v` signature (`v` in `{27, 28}`, low-s) over `hash`.
pub fn eoa_sig(key: &SigningKey, hash: B256) -> Bytes {
    let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
    let mut out = vec![0u8; 65];
    out[..64].copy_from_slice(&signature.to_bytes());
    out[64] = recid.to_byte() + 27;
    Bytes::from(out)
}

/// A canonical EOA self-pay [`TxEip8130`]: no explicit sender/payer, no account
/// changes, and no calls. The sender both authorizes and funds the transaction.
pub fn base_eip8130_tx() -> TxEip8130 {
    TxEip8130 {
        chain_id: CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        expiry: 0,
        max_priority_fee_per_gas: 1_000_000_000,
        max_fee_per_gas: 5_000_000_000,
        gas_limit: 1_000_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: None,
    }
}
