use std::collections::BTreeMap;

use alloy_rlp::{Decodable, Error as DecoderError};
use bytes::Bytes;

use super::{
    EnrKey, EnrKeyUnambiguous, EnrPublicKey, SigningError,
    ed25519_dalek::{self as ed25519, Signer as _, Verifier as _},
};
use crate::Key;

/// The ENR key that stores the public key in the ENR record.
pub const ENR_KEY: &str = "ed25519";

impl EnrKey for ed25519::SigningKey {
    type PublicKey = ed25519::VerifyingKey;

    /// Performs ENR-specific signing.
    ///
    /// Using `ed25519` keys do not currently follow the `v4` identity scheme, which dictates
    /// `secp256k1` keys should be used.
    fn sign_v4(&self, msg: &[u8]) -> Result<Vec<u8>, SigningError> {
        Ok(self.sign(msg).to_bytes().to_vec())
    }

    /// Returns the public key associated with the private key.
    fn public(&self) -> Self::PublicKey {
        self.verifying_key()
    }

    /// Decodes the raw bytes of an ENR's content into a public key if possible.
    fn enr_to_public(content: &BTreeMap<Key, Bytes>) -> Result<Self::PublicKey, DecoderError> {
        let pubkey_bytes =
            content.get(ENR_KEY.as_bytes()).ok_or(DecoderError::Custom("Unknown signature"))?;

        // Decode the RLP
        let pubkey_bytes = Bytes::decode(&mut pubkey_bytes.as_ref())?;

        Self::decode_public(&pubkey_bytes)
    }
}

impl EnrKeyUnambiguous for ed25519::SigningKey {
    fn decode_public(bytes: &[u8]) -> Result<Self::PublicKey, DecoderError> {
        ed25519::VerifyingKey::try_from(bytes)
            .map_err(|_| DecoderError::Custom("Invalid ed25519 Signature"))
    }
}

impl EnrPublicKey for ed25519::VerifyingKey {
    type Raw = [u8; ed25519::PUBLIC_KEY_LENGTH];
    type RawUncompressed = [u8; ed25519::PUBLIC_KEY_LENGTH];

    /// Verify a raw message, given a public key for the v4 identity scheme.
    fn verify_v4(&self, msg: &[u8], sig: &[u8]) -> bool {
        ed25519::Signature::try_from(sig).and_then(|s| self.verify(msg, &s)).is_ok()
    }

    /// Encodes the public key into compressed form, if possible.
    fn encode(&self) -> Self::Raw {
        self.to_bytes()
    }

    /// Encodes the public key in uncompressed form. This is the same as the compressed form for
    /// ed25519 keys
    fn encode_uncompressed(&self) -> Self::RawUncompressed {
        self.encode()
    }

    /// Generates the ENR public key string associated with the ed25519 key type.
    fn enr_key(&self) -> Key {
        ENR_KEY.into()
    }
}
