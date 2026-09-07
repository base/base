//! An implementation for `EnrKey` for `k256::ecdsa::SigningKey`

use std::collections::BTreeMap;

use alloy_rlp::{Decodable, Error as DecoderError};
use bytes::Bytes;
use k256::{
    AffinePoint, CompressedPoint, EncodedPoint,
    ecdsa::{
        Signature, SigningKey, VerifyingKey,
        signature::{DigestVerifier, RandomizedDigestSigner},
    },
    elliptic_curve::{
        point::DecompressPoint,
        sec1::{Coordinates, ToEncodedPoint},
        subtle::Choice,
    },
};
use rand_08::rngs::OsRng;
use sha3::{Digest, Keccak256};

use super::{EnrKey, EnrKeyUnambiguous, EnrPublicKey, SigningError};
use crate::Key;

/// The ENR key that stores the public key in the ENR record.
pub const ENR_KEY: &str = "secp256k1";

impl EnrKey for SigningKey {
    type PublicKey = VerifyingKey;

    fn sign_v4(&self, msg: &[u8]) -> Result<Vec<u8>, SigningError> {
        // take a keccak256 hash then sign.
        let digest = Keccak256::new().chain_update(msg);
        let signature: Signature = self
            .try_sign_digest_with_rng(&mut OsRng, digest)
            .map_err(|_| SigningError::new("failed to sign"))?;

        Ok(signature.to_vec())
    }

    fn public(&self) -> Self::PublicKey {
        *self.verifying_key()
    }

    fn enr_to_public(content: &BTreeMap<Key, Bytes>) -> Result<Self::PublicKey, DecoderError> {
        let pubkey_bytes =
            content.get(ENR_KEY.as_bytes()).ok_or(DecoderError::Custom("Unknown signature"))?;

        // Decode the RLP
        let pubkey_bytes = Bytes::decode(&mut pubkey_bytes.as_ref())?;

        Self::decode_public(&pubkey_bytes)
    }
}

impl EnrKeyUnambiguous for SigningKey {
    fn decode_public(bytes: &[u8]) -> Result<Self::PublicKey, DecoderError> {
        // should be encoded in compressed form, i.e 33 byte raw secp256k1 public key
        VerifyingKey::from_sec1_bytes(bytes)
            .map_err(|_| DecoderError::Custom("Invalid Secp256k1 Signature"))
    }
}

impl EnrPublicKey for VerifyingKey {
    type Raw = CompressedPoint;
    type RawUncompressed = [u8; 64];

    fn verify_v4(&self, msg: &[u8], sig: &[u8]) -> bool {
        if let Ok(sig) = k256::ecdsa::Signature::try_from(sig) {
            return self.verify_digest(Keccak256::new().chain_update(msg), &sig).is_ok();
        }
        false
    }

    fn encode(&self) -> Self::Raw {
        // serialize in compressed form: 33 bytes
        self.into()
    }

    fn encode_uncompressed(&self) -> Self::RawUncompressed {
        let p = EncodedPoint::from(self);
        let (x, y) = match p.coordinates() {
            Coordinates::Compact { .. } | Coordinates::Identity => unreachable!(),
            Coordinates::Compressed { x, y_is_odd } => (
                x,
                *AffinePoint::decompress(x, Choice::from(u8::from(y_is_odd)))
                    .unwrap()
                    .to_encoded_point(false)
                    .y()
                    .unwrap(),
            ),
            Coordinates::Uncompressed { x, y } => (x, *y),
        };

        let mut coords = [0; 64];
        coords[..32].copy_from_slice(x);
        coords[32..].copy_from_slice(&y);

        coords
    }

    fn enr_key(&self) -> Key {
        ENR_KEY.into()
    }
}
