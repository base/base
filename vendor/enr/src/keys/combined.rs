//! An implementation that combines the currently supported key types. This
//! facilitates and ENR type than can decode and read ENR's of all supported key types.
//!
//! Currently only `secp256k1` and `ed25519` key types are supported.

use std::collections::BTreeMap;

use alloy_rlp::Error as DecoderError;
use bytes::Bytes;
use zeroize::Zeroize;

use super::{EnrKey, EnrPublicKey, SigningError, ed25519_dalek as ed25519};
use crate::Key;

/// A standard implementation of the `EnrKey` trait used to sign and modify ENR records. The variants here represent the currently
/// supported in-built signing schemes.
pub enum CombinedKey {
    /// An `secp256k1` keypair.
    Secp256k1(k256::ecdsa::SigningKey),
    /// An `Ed25519` keypair.
    Ed25519(ed25519::SigningKey),
}

impl From<k256::ecdsa::SigningKey> for CombinedKey {
    fn from(secret_key: k256::ecdsa::SigningKey) -> Self {
        Self::Secp256k1(secret_key)
    }
}

impl From<ed25519::SigningKey> for CombinedKey {
    fn from(keypair: ed25519_dalek::SigningKey) -> Self {
        Self::Ed25519(keypair)
    }
}

impl EnrKey for CombinedKey {
    type PublicKey = CombinedPublicKey;

    /// Performs ENR-specific signing.
    ///
    /// Note: that this library supports a number of signing algorithms. The ENR specification
    /// currently lists the `v4` identity scheme which requires the `secp256k1` signing algorithm.
    /// Using `secp256k1` keys follow the `v4` identity scheme, using other types do not, although
    /// they are supported.
    fn sign_v4(&self, msg: &[u8]) -> Result<Vec<u8>, SigningError> {
        match self {
            Self::Secp256k1(key) => key.sign_v4(msg),
            Self::Ed25519(key) => key.sign_v4(msg),
        }
    }

    /// Returns the public key associated with the private key.
    fn public(&self) -> Self::PublicKey {
        match self {
            Self::Secp256k1(key) => CombinedPublicKey::from(key.public()),
            Self::Ed25519(key) => CombinedPublicKey::from(key.public()),
        }
    }

    /// Decodes the raw bytes of an ENR's content into a public key if possible.
    fn enr_to_public(content: &BTreeMap<Key, Bytes>) -> Result<Self::PublicKey, DecoderError> {
        k256::ecdsa::SigningKey::enr_to_public(content)
            .map(CombinedPublicKey::Secp256k1)
            .or_else(|_| ed25519::SigningKey::enr_to_public(content).map(CombinedPublicKey::from))
    }
}

impl CombinedKey {
    /// Generates a new secp256k1 key.
    #[must_use]
    pub fn generate_secp256k1() -> Self {
        let key = k256::ecdsa::SigningKey::random(&mut rand_08::thread_rng());
        Self::Secp256k1(key)
    }

    /// Generates a new ed25510 key.
    #[must_use]
    pub fn generate_ed25519() -> Self {
        Self::Ed25519(ed25519::SigningKey::generate(&mut rand_08::thread_rng()))
    }

    /// Imports a secp256k1 from raw bytes in any format.
    pub fn secp256k1_from_bytes(bytes: &mut [u8]) -> Result<Self, DecoderError> {
        let key = k256::ecdsa::SigningKey::from_slice(bytes)
            .map_err(|_| DecoderError::Custom("Invalid secp256k1 secret key"))
            .map(Self::from)?;
        bytes.zeroize();
        Ok(key)
    }

    /// Imports an ed25519 key from raw 32 bytes.
    pub fn ed25519_from_bytes(bytes: &mut [u8]) -> Result<Self, DecoderError> {
        #[allow(clippy::useless_asref)]
        let key = ed25519::SigningKey::try_from(bytes.as_ref())
            .map_err(|_| DecoderError::Custom("Invalid ed25519 secret key"))
            .map(Self::from)?;
        bytes.zeroize();
        Ok(key)
    }

    /// Encodes the `CombinedKey` into compressed (where possible) bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Secp256k1(key) => key.to_bytes().to_vec(),
            Self::Ed25519(key) => key.to_bytes().to_vec(),
        }
    }
}

/// A combined implementation of `EnrPublicKey` which has support for `Secp256k1`
/// and `Ed25519` for ENR signature verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CombinedPublicKey {
    /// An `Secp256k1` public key.
    Secp256k1(k256::ecdsa::VerifyingKey),
    /// An `Ed25519` public key.
    Ed25519(ed25519::VerifyingKey),
}

impl From<k256::ecdsa::VerifyingKey> for CombinedPublicKey {
    fn from(public_key: k256::ecdsa::VerifyingKey) -> Self {
        Self::Secp256k1(public_key)
    }
}

impl From<ed25519::VerifyingKey> for CombinedPublicKey {
    fn from(public_key: ed25519::VerifyingKey) -> Self {
        Self::Ed25519(public_key)
    }
}

impl EnrPublicKey for CombinedPublicKey {
    type Raw = Vec<u8>;
    type RawUncompressed = Vec<u8>;

    /// Verify a raw message, given a public key for the v4 identity scheme.
    fn verify_v4(&self, msg: &[u8], sig: &[u8]) -> bool {
        match self {
            Self::Secp256k1(pk) => pk.verify_v4(msg, sig),
            Self::Ed25519(pk) => pk.verify_v4(msg, sig),
        }
    }

    /// Encodes the public key into compressed form, if possible.
    fn encode(&self) -> Vec<u8> {
        match self {
            // serialize in compressed form: 33 bytes
            Self::Secp256k1(pk) => pk.encode().to_vec(),
            Self::Ed25519(pk) => pk.encode().to_vec(),
        }
    }

    /// Encodes the public key in uncompressed form.
    fn encode_uncompressed(&self) -> Vec<u8> {
        match self {
            Self::Secp256k1(pk) => pk.encode_uncompressed().to_vec(),
            Self::Ed25519(pk) => pk.encode_uncompressed().to_vec(),
        }
    }

    /// Generates the ENR public key string associated with the key type.
    fn enr_key(&self) -> Key {
        match self {
            Self::Secp256k1(key) => key.enr_key(),
            Self::Ed25519(key) => key.enr_key(),
        }
    }
}
