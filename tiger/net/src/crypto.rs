//! Cryptographic primitives shared by RLPx and discv5: node keys, ECDH, hashes, and ciphers.
//!
//! Everything is a thin call into a small audited crate (secp256k1, `ring` for SHA-256, HMAC,
//! HKDF and AES-GCM, RustCrypto `aes` and `ctr` for the AES-CTR that `ring` lacks).

use aes::cipher::{BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use alloy_primitives::{B256, B512};
use secp256k1::{
    Message, PublicKey, SECP256K1, SecretKey,
    ecdsa::{RecoverableSignature, RecoveryId},
};
use ring::{aead, digest, hmac};

/// A node's identity on the network: the 64-byte uncompressed secp256k1 public key without the
/// `0x04` prefix, as used in enode URLs and the RLPx handshake.
pub type PeerId = B512;

/// Fills `buf` with operating-system randomness.
pub(crate) fn random_bytes(buf: &mut [u8]) {
    getrandom::fill(buf).expect("operating system randomness is available");
}

/// Returns `N` bytes of operating-system randomness.
pub(crate) fn random_array<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    random_bytes(&mut out);
    out
}

/// Returns a fresh random secp256k1 secret key.
pub(crate) fn random_secret_key() -> SecretKey {
    loop {
        if let Ok(key) = SecretKey::from_byte_array(random_array()) {
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
    let mut context = digest::Context::new(&digest::SHA256);
    for part in parts {
        context.update(part);
    }
    context.finish().as_ref().try_into().expect("SHA-256 digests are 32 bytes")
}

/// HMAC-SHA-256 of the concatenation of `parts`.
pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut context = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    for part in parts {
        context.update(part);
    }
    context.sign().as_ref().try_into().expect("HMAC-SHA-256 tags are 32 bytes")
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

/// AES-128-GCM encryption (discv5 message packets): returns ciphertext with the 16-byte tag.
pub(crate) fn aes128_gcm_encrypt(
    key: &[u8; 16],
    nonce: &[u8; 12],
    plain: &[u8],
    aad: &[u8],
) -> Vec<u8> {
    let mut out = plain.to_vec();
    aes128_gcm_key(key)
        .seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(*nonce), aead::Aad::from(aad), &mut out)
        .expect("AES-GCM encryption of a bounded packet cannot fail");
    out
}

/// AES-128-GCM decryption; `None` if the tag does not authenticate.
pub(crate) fn aes128_gcm_decrypt(
    key: &[u8; 16],
    nonce: &[u8; 12],
    cipher_text: &[u8],
    aad: &[u8],
) -> Option<Vec<u8>> {
    let mut out = cipher_text.to_vec();
    let plain_len = aes128_gcm_key(key)
        .open_in_place(aead::Nonce::assume_unique_for_key(*nonce), aead::Aad::from(aad), &mut out)
        .ok()?
        .len();
    out.truncate(plain_len);
    Some(out)
}

/// The AES-128-GCM key for one packet; discv5 session keys change per peer, so none is kept.
fn aes128_gcm_key(key: &[u8; 16]) -> aead::LessSafeKey {
    aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, key).expect("AES-128 keys are 16 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    #[test]
    fn recoverable_signature_round_trips() {
        let key = random_secret_key();
        let digest = keccak256(b"tiger");
        let sig = sign_recoverable(digest, &key);
        let public = recover(digest, &sig).unwrap();
        assert_eq!(peer_id(&public), peer_id_of_secret(&key));
        assert_eq!(public_key(&peer_id(&public)).unwrap(), public);
    }
}
