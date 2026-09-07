use std::collections::BTreeMap;

use alloy_rlp::{Decodable, Error as DecoderError};
use bytes::Bytes;
use rand_08::RngCore;
#[cfg(not(test))]
use rand_08::rngs::OsRng;
use secp256k1::SECP256K1;

#[cfg(test)]
use self::MockOsRng as OsRng;
use super::{EnrKey, EnrKeyUnambiguous, EnrPublicKey, SigningError};
use crate::{Key, digest};

/// The ENR key that stores the public key in the ENR record.
pub const ENR_KEY: &str = "secp256k1";

impl EnrKey for secp256k1::SecretKey {
    type PublicKey = secp256k1::PublicKey;

    fn sign_v4(&self, msg: &[u8]) -> Result<Vec<u8>, SigningError> {
        // take a keccak256 hash then sign.
        let hash = digest(msg);
        let m = secp256k1::Message::from_digest(hash);
        // serialize to an uncompressed 64 byte vector
        let signature = {
            let mut noncedata = [0; 32];
            OsRng.fill_bytes(&mut noncedata);
            SECP256K1.sign_ecdsa_with_noncedata(&m, self, &noncedata)
        };
        Ok(signature.serialize_compact().to_vec())
    }

    fn public(&self) -> Self::PublicKey {
        Self::PublicKey::from_secret_key(SECP256K1, self)
    }

    fn enr_to_public(content: &BTreeMap<Key, Bytes>) -> Result<Self::PublicKey, DecoderError> {
        let pubkey_bytes =
            content.get(ENR_KEY.as_bytes()).ok_or(DecoderError::Custom("Unknown signature"))?;
        // Decode the RLP
        let pubkey_bytes = Bytes::decode(&mut pubkey_bytes.as_ref())?;

        Self::decode_public(&pubkey_bytes)
    }
}

impl EnrKeyUnambiguous for secp256k1::SecretKey {
    fn decode_public(bytes: &[u8]) -> Result<Self::PublicKey, DecoderError> {
        // should be encoded in compressed form, i.e 33 byte raw secp256k1 public key
        secp256k1::PublicKey::from_slice(bytes)
            .map_err(|_| DecoderError::Custom("Invalid Secp256k1 Signature"))
    }
}

impl EnrPublicKey for secp256k1::PublicKey {
    type Raw = [u8; secp256k1::constants::PUBLIC_KEY_SIZE];
    type RawUncompressed = [u8; secp256k1::constants::UNCOMPRESSED_PUBLIC_KEY_SIZE - 1];

    fn verify_v4(&self, msg: &[u8], sig: &[u8]) -> bool {
        let msg = digest(msg);
        if let Ok(sig) = secp256k1::ecdsa::Signature::from_compact(sig) {
            let msg = secp256k1::Message::from_digest(msg);
            return SECP256K1.verify_ecdsa(&msg, &sig, self).is_ok();
        }
        false
    }

    fn encode(&self) -> Self::Raw {
        self.serialize()
    }

    fn encode_uncompressed(&self) -> Self::RawUncompressed {
        let mut out = [0_u8; secp256k1::constants::UNCOMPRESSED_PUBLIC_KEY_SIZE - 1];
        out.copy_from_slice(&self.serialize_uncompressed()[1..]);
        out
    }

    fn enr_key(&self) -> Key {
        ENR_KEY.into()
    }
}

#[cfg(test)]
const MOCK_ECDSA_NONCE_ADDITIONAL_DATA: [u8; 32] = [
    // 0xbaaaaaad...
    0xba, 0xaa, 0xaa, 0xad, 0xba, 0xaa, 0xaa, 0xad, 0xba, 0xaa, 0xaa, 0xad, 0xba, 0xaa, 0xaa, 0xad,
    0xba, 0xaa, 0xaa, 0xad, 0xba, 0xaa, 0xaa, 0xad, 0xba, 0xaa, 0xaa, 0xad, 0xba, 0xaa, 0xaa, 0xad,
];

#[cfg(test)]
struct MockOsRng;

#[cfg(test)]
impl RngCore for MockOsRng {
    fn next_u32(&mut self) -> u32 {
        unimplemented!();
    }

    fn next_u64(&mut self) -> u64 {
        unimplemented!();
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        debug_assert_eq!(dest.len(), MOCK_ECDSA_NONCE_ADDITIONAL_DATA.len());
        dest.copy_from_slice(&MOCK_ECDSA_NONCE_ADDITIONAL_DATA);
    }

    fn try_fill_bytes(&mut self, _dest: &mut [u8]) -> Result<(), rand_08::Error> {
        unimplemented!();
    }
}
