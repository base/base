//! secp256k1 ECDSA signer recovery with EIP-2 low-`s` enforcement.

use alloy_primitives::{Address, B256, keccak256};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

use crate::CryptoError;

/// secp256k1 ECDSA operations shared across the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Secp256k1;

impl Secp256k1 {
    /// Recovers the Ethereum address that signed `hash` from a 65-byte
    /// `r || s || v` signature, requiring `v in {27, 28}` and enforcing **EIP-2
    /// low-`s`** (a malleable upper-half-`s` signature is rejected, not
    /// canonicalized).
    ///
    /// This is the recovery semantics of the enshrined EIP-8130 k1 authenticator
    /// and the deployed `AccountConfiguration` reference. It deliberately differs
    /// from the EVM `ecrecover` precompile, which normalizes and accepts a
    /// high-`s` signature.
    ///
    /// Returns [`CryptoError::MalformedSignature`] when `signature` is not 65
    /// bytes, and [`CryptoError::InvalidSignature`] when `v` is out of range, the
    /// signature is malleable, or recovery fails.
    pub fn recover(hash: B256, signature: &[u8]) -> Result<Address, CryptoError> {
        if signature.len() != 65 {
            return Err(CryptoError::MalformedSignature);
        }
        let recovery = match signature[64] {
            27 | 28 => signature[64] - 27,
            _ => return Err(CryptoError::InvalidSignature),
        };
        let sig =
            Signature::from_slice(&signature[..64]).map_err(|_| CryptoError::InvalidSignature)?;
        // `normalize_s` returns `Some` only when `s` is in the upper half, i.e. a
        // malleable high-`s` signature: reject it rather than canonicalizing.
        if sig.normalize_s().is_some() {
            return Err(CryptoError::InvalidSignature);
        }
        let recovery_id = RecoveryId::from_byte(recovery).ok_or(CryptoError::InvalidSignature)?;
        let key = VerifyingKey::recover_from_prehash(hash.as_slice(), &sig, recovery_id)
            .map_err(|_| CryptoError::InvalidSignature)?;
        let encoded = key.to_encoded_point(false);
        // encoded = 0x04 || x(32) || y(32); address = keccak256(x || y)[12..].
        Ok(Address::from_slice(&keccak256(&encoded.as_bytes()[1..])[12..]))
    }
}

#[cfg(test)]
mod tests {
    use k256::ecdsa::{Signature as K256Signature, SigningKey};

    use super::*;

    const HASH: B256 = B256::repeat_byte(0x42);

    fn signing_key() -> SigningKey {
        SigningKey::from_slice(&[0x11u8; 32]).unwrap()
    }

    fn address_of(key: &SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    /// 65-byte `r || s || v` signature over `hash`, `v` in `{27, 28}` (low-s).
    fn sign(key: &SigningKey, hash: B256) -> [u8; 65] {
        let (sig, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    #[test]
    fn recovers_the_signer_address() {
        let key = signing_key();
        let recovered = Secp256k1::recover(HASH, &sign(&key, HASH)).unwrap();
        assert_eq!(recovered, address_of(&key));
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(Secp256k1::recover(HASH, &[0u8; 64]), Err(CryptoError::MalformedSignature));
    }

    #[test]
    fn rejects_v_not_27_or_28() {
        let mut sig = sign(&signing_key(), HASH);
        sig[64] -= 27; // 0 or 1: invalid for the EVM ecrecover sentinel.
        assert_eq!(Secp256k1::recover(HASH, &sig), Err(CryptoError::InvalidSignature));
    }

    #[test]
    fn rejects_high_s() {
        // The malleable upper-half-`s` counterpart (negate `s`, flip recovery
        // parity) recovers the same signer but MUST be rejected.
        let key = signing_key();
        let (sig, recid) = key.sign_prehash_recoverable(HASH.as_slice()).unwrap();
        let s_high = -*sig.s();
        let high = K256Signature::from_scalars(sig.r().to_bytes(), s_high.to_bytes()).unwrap();
        let mut bytes = [0u8; 65];
        bytes[..64].copy_from_slice(&high.to_bytes());
        bytes[64] = (recid.to_byte() ^ 1) + 27;
        assert_eq!(Secp256k1::recover(HASH, &bytes), Err(CryptoError::InvalidSignature));
    }
}
