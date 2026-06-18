//! secp256r1 (P-256) ECDSA verification with low-`s` enforcement.

use p256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};

use crate::CryptoError;

/// secp256r1 (P-256) ECDSA operations shared across the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Secp256r1;

impl Secp256r1 {
    /// Verifies a P-256 signature `(r, s)` over the `prehash` for the public key
    /// `(x, y)`, enforcing low-`s` to match `OpenZeppelin` `P256.verify` (the
    /// malleability check the deployed EIP-8130 P-256 and `WebAuthn`
    /// authenticators perform).
    ///
    /// Each of `r`, `s`, `x`, and `y` must be exactly 32 bytes. Returns
    /// [`CryptoError::InvalidPublicKey`] when `(x, y)` is the wrong length or not
    /// on the curve, and [`CryptoError::InvalidSignature`] when `(r, s)` is the
    /// wrong length, malleable, or does not verify.
    pub fn verify_prehash(
        prehash: &[u8],
        r: &[u8],
        s: &[u8],
        x: &[u8],
        y: &[u8],
    ) -> Result<(), CryptoError> {
        if x.len() != 32 || y.len() != 32 {
            return Err(CryptoError::InvalidPublicKey);
        }
        if r.len() != 32 || s.len() != 32 {
            return Err(CryptoError::InvalidSignature);
        }

        let mut sec1 = [0u8; 65];
        sec1[0] = 0x04;
        sec1[1..33].copy_from_slice(x);
        sec1[33..65].copy_from_slice(y);
        let key =
            VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| CryptoError::InvalidPublicKey)?;

        let mut rs = [0u8; 64];
        rs[..32].copy_from_slice(r);
        rs[32..].copy_from_slice(s);
        let signature = Signature::from_slice(&rs).map_err(|_| CryptoError::InvalidSignature)?;
        // Reject a malleable upper-half-`s` signature rather than canonicalizing.
        if signature.normalize_s().is_some() {
            return Err(CryptoError::InvalidSignature);
        }
        key.verify_prehash(prehash, &signature).map_err(|_| CryptoError::InvalidSignature)
    }
}

#[cfg(test)]
mod tests {
    use p256::ecdsa::{Signature as P256Sig, SigningKey, signature::hazmat::PrehashSigner};

    use super::*;

    const PREHASH: &[u8; 32] = &[0x42u8; 32];

    fn signing_key() -> SigningKey {
        SigningKey::from_slice(&[0x22u8; 32]).unwrap()
    }

    fn public_xy(key: &SigningKey) -> ([u8; 32], [u8; 32]) {
        let point = key.verifying_key().to_encoded_point(false);
        let bytes = point.as_bytes();
        (bytes[1..33].try_into().unwrap(), bytes[33..65].try_into().unwrap())
    }

    /// Low-`s`-normalized `(r, s)` over `prehash`.
    fn sign(key: &SigningKey, prehash: &[u8]) -> [u8; 64] {
        let sig: P256Sig = key.sign_prehash(prehash).unwrap();
        sig.normalize_s().unwrap_or(sig).to_bytes().into()
    }

    #[test]
    fn verifies_a_valid_signature() {
        let key = signing_key();
        let (x, y) = public_xy(&key);
        let rs = sign(&key, PREHASH);
        assert_eq!(Secp256r1::verify_prehash(PREHASH, &rs[..32], &rs[32..], &x, &y), Ok(()));
    }

    #[test]
    fn rejects_a_tampered_prehash() {
        let key = signing_key();
        let (x, y) = public_xy(&key);
        let rs = sign(&key, PREHASH);
        let other = [0x99u8; 32];
        assert_eq!(
            Secp256r1::verify_prehash(&other, &rs[..32], &rs[32..], &x, &y),
            Err(CryptoError::InvalidSignature),
        );
    }

    #[test]
    fn rejects_high_s() {
        // Force the upper-half-`s` (malleable) counterpart of a valid signature.
        let key = signing_key();
        let (x, y) = public_xy(&key);
        let sig: P256Sig = key.sign_prehash(PREHASH).unwrap();
        let low = sig.normalize_s().unwrap_or(sig);
        let high = P256Sig::from_scalars(low.r(), -low.s()).unwrap();
        let rs: [u8; 64] = high.to_bytes().into();
        assert_eq!(
            Secp256r1::verify_prehash(PREHASH, &rs[..32], &rs[32..], &x, &y),
            Err(CryptoError::InvalidSignature),
        );
    }

    #[test]
    fn rejects_wrong_length_public_key() {
        let rs = [0u8; 64];
        assert_eq!(
            Secp256r1::verify_prehash(PREHASH, &rs[..32], &rs[32..], &[0u8; 31], &[0u8; 32]),
            Err(CryptoError::InvalidPublicKey),
        );
    }
}
