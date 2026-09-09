//! Utility functions for hashing and encoding.

use alloy_primitives::B256;
use hmac::{Hmac, Mac};
use secp256k1::{PublicKey, SecretKey};
use sha2::{Digest, Sha256};

/// Cryptographic operations used by the RLPx encrypted transport.
#[derive(Debug)]
pub struct EciesCrypto;

impl EciesCrypto {
    /// Hashes the input data with SHA256 - <https://en.wikipedia.org/wiki/SHA-2>
    pub fn sha256(data: &[u8]) -> B256 {
        B256::from(Sha256::digest(data).as_ref())
    }

    /// Produces a `HMAC_SHA256` digest of the `input_data` and `auth_data` with the given `key`.
    /// This is done by accumulating each slice in `input_data` into the HMAC state, then accumulating
    /// the `auth_data` and returning the resulting digest.
    pub fn hmac_sha256(key: &[u8], input: &[&[u8]], auth_data: &[u8]) -> B256 {
        let mut hmac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        for input in input {
            hmac.update(input);
        }
        hmac.update(auth_data);
        B256::from_slice(&hmac.finalize().into_bytes())
    }
    /// Computes the shared secret with ECDH and strips the y coordinate after computing the shared
    /// secret.
    ///
    /// This uses the given remote public key and local (ephemeral) secret key to [compute a shared
    /// secp256k1 point](secp256k1::ecdh::shared_secret_point) and slices off the y coordinate from the
    /// returned pair, returning only the bytes of the x coordinate as a [`B256`].
    pub fn ecdh_x(public_key: &PublicKey, secret_key: &SecretKey) -> B256 {
        B256::from_slice(&secp256k1::ecdh::shared_secret_point(public_key, secret_key)[..32])
    }

    /// This is the NIST SP 800-56A Concatenation Key Derivation Function (KDF) using SHA-256.
    ///
    /// Internally this uses [`concat_kdf::derive_key_into`] to derive a key into the given `dest`
    /// slice.
    ///
    /// # Panics
    /// * If the `dest` is empty
    /// * If the `dest` len is greater than or equal to the hash output len * the max counter value. In
    ///   this case, the hash output len is 32 bytes, and the max counter value is 2^32 - 1. So the dest
    ///   cannot have a len greater than 32 * 2^32 - 1.
    pub fn kdf(secret: B256, s1: &[u8], dest: &mut [u8]) {
        concat_kdf::derive_key_into::<Sha256>(secret.as_slice(), s1, dest).unwrap();
    }
}
