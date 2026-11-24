//! Cryptographic primitives shared by RLPx and discv5: node keys, ECDH, hashes, and ciphers.
//!
//! Everything is a thin call into a small audited crate (secp256k1, RustCrypto `aes`, `ctr`,
//! `sha2`, `hmac`, `aes-gcm`) or keccak from alloy-primitives; HKDF and the NIST concat KDF are
//! written here because each is a few lines over HMAC/SHA-256.

use aes::cipher::{BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use alloy_primitives::{B256, B512, keccak256};
use hmac::{Hmac, Mac};
use secp256k1::{
    Message, PublicKey, SECP256K1, SecretKey,
    ecdsa::{RecoverableSignature, RecoveryId},
};
use sha2::{Digest, Sha256};

/// A node's identity on the network: the 64-byte uncompressed secp256k1 public key without the
/// `0x04` prefix, as used in enode URLs and the RLPx handshake.
pub type PeerId = B512;

/// Fills `buf` with operating-system randomness.
pub(crate) fn random_bytes(buf: &mut [u8]) {
    getrandom::fill(buf).expect("operating system randomness is available");
}

/// Returns a fresh random secp256k1 secret key.
pub(crate) fn random_secret_key() -> SecretKey {
    loop {
        let mut bytes = [0u8; 32];
        random_bytes(&mut bytes);
        if let Ok(key) = SecretKey::from_byte_array(bytes) {
            return key;
        }
    }
}

/// Returns the [`PeerId`] of a public key.
pub(crate) fn peer_id(public: &PublicKey) -> PeerId {
    let bytes = public.serialize_uncompressed();
    PeerId::from_slice(&bytes[1..])
}

/// Returns the [`PeerId`] of a secret key.
pub(crate) fn peer_id_of_secret(secret: &SecretKey) -> PeerId {
    peer_id(&PublicKey::from_secret_key_global(secret))
}

/// Parses a [`PeerId`] back into a public key, failing for a point not on the curve.
pub(crate) fn public_key(id: &PeerId) -> Option<PublicKey> {
    let mut bytes = [4u8; 65];
    bytes[1..].copy_from_slice(id.as_slice());
    PublicKey::from_byte_array_uncompressed(bytes).ok()
}

/// The x coordinate of the ECDH shared point, the shared secret used by RLPx and discv5.
pub(crate) fn ecdh_x(public: &PublicKey, secret: &SecretKey) -> B256 {
    let point = secp256k1::ecdh::shared_secret_point(public, secret);
    B256::from_slice(&point[..32])
}

/// Signs a 32-byte digest, returning `r || s || v` with `v` in {0, 1}.
pub(crate) fn sign_recoverable(digest: B256, secret: &SecretKey) -> [u8; 65] {
    let sig = SECP256K1.sign_ecdsa_recoverable(Message::from_digest(digest.0), secret);
    let (id, compact) = sig.serialize_compact();
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&compact);
    out[64] = i32::from(id) as u8;
    out
}

/// Recovers the signer of a `r || s || v` signature over a 32-byte digest.
pub(crate) fn recover(digest: B256, sig: &[u8; 65]) -> Option<PublicKey> {
    let id = RecoveryId::try_from(sig[64] as i32).ok()?;
    let sig = RecoverableSignature::from_compact(&sig[..64], id).ok()?;
    SECP256K1.recover_ecdsa(Message::from_digest(digest.0), &sig).ok()
}

/// Signs a 32-byte digest, returning the 64-byte compact `r || s` signature (ENR v4, discv5).
pub(crate) fn sign_compact(digest: B256, secret: &SecretKey) -> [u8; 64] {
    SECP256K1.sign_ecdsa(Message::from_digest(digest.0), secret).serialize_compact()
}

/// Verifies a 64-byte compact signature over a 32-byte digest.
pub(crate) fn verify_compact(digest: B256, sig: &[u8], public: &PublicKey) -> bool {
    let Ok(mut sig) = secp256k1::ecdsa::Signature::from_compact(sig) else { return false };
    sig.normalize_s();
    SECP256K1.verify_ecdsa(Message::from_digest(digest.0), &sig, public).is_ok()
}

/// SHA-256 of the concatenation of `parts`.
pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// HMAC-SHA-256 of the concatenation of `parts`.
pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// HKDF-SHA-256 (RFC 5869) extract and expand, writing `out.len()` bytes (at most 255 blocks).
pub(crate) fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) {
    assert!(out.len() <= 255 * 32);
    let prk = hmac_sha256(salt, &[ikm]);
    let mut previous: Vec<u8> = Vec::new();
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let block = hmac_sha256(&prk, &[&previous, info, &[i as u8 + 1]]);
        chunk.copy_from_slice(&block[..chunk.len()]);
        previous = block.to_vec();
    }
}

/// The NIST SP 800-56 concatenation KDF with SHA-256 that RLPx's ECIES uses, for 32 bytes.
pub(crate) fn concat_kdf_32(secret: &B256) -> [u8; 32] {
    sha256(&[&1u32.to_be_bytes(), secret.as_slice()])
}

/// Keccak-256 of the concatenation of `parts`.
pub(crate) fn keccak(parts: &[&[u8]]) -> B256 {
    if parts.len() == 1 {
        return keccak256(parts[0]);
    }
    keccak256(parts.concat())
}

/// AES-128 in CTR mode with a big-endian 128-bit counter, applied in place.
pub(crate) fn aes128_ctr(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    let mut cipher = ctr::Ctr128BE::<aes::Aes128>::new(key.into(), iv.into());
    cipher.apply_keystream(data);
}

/// AES-256 CTR keystream with a zero IV that persists across calls, as RLPx frames need.
pub(crate) struct Aes256Ctr(ctr::Ctr128BE<aes::Aes256>);

impl Aes256Ctr {
    /// Creates the stream for `key` with a zero IV.
    pub(crate) fn new(key: &B256) -> Self {
        Self(ctr::Ctr128BE::<aes::Aes256>::new(key.0.as_slice().into(), &[0u8; 16].into()))
    }

    /// Encrypts or decrypts `data` in place, advancing the keystream.
    pub(crate) fn apply(&mut self, data: &mut [u8]) {
        self.0.apply_keystream(data);
    }
}

impl std::fmt::Debug for Aes256Ctr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Aes256Ctr")
    }
}

/// AES-256 single-block encryption (ECB of one block), used by the RLPx frame MAC.
pub(crate) struct Aes256Block(aes::Aes256);

impl Aes256Block {
    /// Creates the block cipher for `key`.
    pub(crate) fn new(key: &B256) -> Self {
        Self(aes::Aes256::new(key.0.as_slice().into()))
    }

    /// Encrypts one 16-byte block in place.
    pub(crate) fn encrypt(&self, block: &mut [u8; 16]) {
        self.0.encrypt_block(block.into());
    }
}

impl std::fmt::Debug for Aes256Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Aes256Block")
    }
}

/// AES-128-GCM encryption (discv5 message packets): returns ciphertext with the 16-byte tag.
pub(crate) fn aes128_gcm_encrypt(key: &[u8; 16], nonce: &[u8; 12], plain: &[u8], aad: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::{Aead, Payload};
    let cipher = aes_gcm::Aes128Gcm::new(key.into());
    cipher
        .encrypt(nonce.into(), Payload { msg: plain, aad })
        .expect("AES-GCM encryption of a bounded packet cannot fail")
}

/// AES-128-GCM decryption; `None` if the tag does not authenticate.
pub(crate) fn aes128_gcm_decrypt(
    key: &[u8; 16],
    nonce: &[u8; 12],
    cipher_text: &[u8],
    aad: &[u8],
) -> Option<Vec<u8>> {
    use aes_gcm::aead::{Aead, Payload};
    let cipher = aes_gcm::Aes128Gcm::new(key.into());
    cipher.decrypt(nonce.into(), Payload { msg: cipher_text, aad }).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    #[test]
    fn hkdf_matches_rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt = hex::decode("000102030405060708090a0b0c").unwrap();
        let info = hex::decode("f0f1f2f3f4f5f6f7f8f9").unwrap();
        let mut okm = [0u8; 42];
        hkdf_sha256(&salt, &ikm, &info, &mut okm);
        assert_eq!(
            hex::encode(okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn recoverable_signature_round_trips() {
        let key = random_secret_key();
        let digest = keccak(&[b"tiger"]);
        let sig = sign_recoverable(digest, &key);
        let public = recover(digest, &sig).unwrap();
        assert_eq!(peer_id(&public), peer_id_of_secret(&key));
        assert_eq!(public_key(&peer_id(&public)).unwrap(), public);
    }
}
