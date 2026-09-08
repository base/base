//! [`k256`] signer implementation.

use std::str::FromStr;

use alloy_primitives::{B256, B512, hex};
use alloy_signer::utils::secret_key_to_address;
use k256::{
    FieldBytes, NonZeroScalar, SecretKey as K256SecretKey,
    ecdsa::{self, SigningKey},
};
use rand_08::{CryptoRng, Rng};

use super::{LocalSignerError, PrivateKeySigner};

impl PrivateKeySigner {
    /// Creates a new [`PrivateKeySigner`] instance from a [`SigningKey`].
    ///
    /// This can also be used to create a [`PrivateKeySigner`] from a [`SecretKey`](K256SecretKey).
    /// See also the `From` implementations.
    #[doc(alias = "from_private_key")]
    #[doc(alias = "new_private_key")]
    #[doc(alias = "new_pk")]
    #[inline]
    pub fn from_signing_key(credential: SigningKey) -> Self {
        let address = secret_key_to_address(&credential);
        Self::new_with_credential(credential, address, None)
    }

    /// Creates a new [`PrivateKeySigner`] instance from a raw scalar serialized as a [`B256`] byte
    /// array.
    ///
    /// This is identical to [`from_field_bytes`](Self::from_field_bytes).
    #[inline]
    pub fn from_bytes(bytes: &B256) -> Result<Self, ecdsa::Error> {
        Self::from_field_bytes((&bytes.0).into())
    }

    /// Creates a new [`PrivateKeySigner`] instance from a raw scalar serialized as a [`FieldBytes`] byte
    /// array.
    #[inline]
    pub fn from_field_bytes(bytes: &FieldBytes) -> Result<Self, ecdsa::Error> {
        SigningKey::from_bytes(bytes).map(Self::from_signing_key)
    }

    /// Creates a new [`PrivateKeySigner`] instance from a raw scalar serialized as a byte slice.
    ///
    /// Byte slices shorter than the field size (32 bytes) are handled by zero padding the input.
    #[inline]
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ecdsa::Error> {
        SigningKey::from_slice(bytes).map(Self::from_signing_key)
    }

    /// Creates a new random keypair seeded with [`rand_08::thread_rng()`].
    #[inline]
    pub fn random() -> Self {
        Self::random_with(&mut rand_08::thread_rng())
    }

    /// Creates a new random keypair seeded with the provided RNG.
    #[inline]
    pub fn random_with<R: Rng + CryptoRng>(rng: &mut R) -> Self {
        Self::from_signing_key(SigningKey::random(rng))
    }

    /// Borrow the secret [`NonZeroScalar`] value for this key.
    ///
    /// # ⚠️ Warning
    ///
    /// This value is key material.
    ///
    /// Please treat it with the care it deserves!
    #[inline]
    pub fn as_nonzero_scalar(&self) -> &NonZeroScalar {
        self.credential.as_nonzero_scalar()
    }

    /// Serializes this [`PrivateKeySigner`]'s [`SigningKey`] as a [`B256`] byte array.
    ///
    /// # Security
    ///
    /// The returned bytes are unencrypted private-key material. Do not log or disclose them, and
    /// clear copies as soon as practical.
    #[inline]
    pub fn to_bytes(&self) -> B256 {
        B256::new(<[u8; 32]>::from(self.to_field_bytes()))
    }

    /// Serializes this [`PrivateKeySigner`]'s [`SigningKey`] as a [`FieldBytes`] byte array.
    ///
    /// # Security
    ///
    /// The returned bytes are unencrypted private-key material. Do not log or disclose them, and
    /// clear copies as soon as practical.
    #[inline]
    pub fn to_field_bytes(&self) -> FieldBytes {
        self.credential.to_bytes()
    }

    /// Convenience function that returns this signer's ethereum public key as a [`B512`] byte
    /// array.
    #[inline]
    pub fn public_key(&self) -> B512 {
        B512::from_slice(&self.credential.verifying_key().to_encoded_point(false).as_bytes()[1..])
    }
}

impl PartialEq for PrivateKeySigner {
    fn eq(&self, other: &Self) -> bool {
        self.credential.to_bytes().eq(&other.credential.to_bytes())
            && self.address == other.address
            && self.chain_id == other.chain_id
    }
}

impl From<SigningKey> for PrivateKeySigner {
    fn from(value: SigningKey) -> Self {
        Self::from_signing_key(value)
    }
}

impl From<K256SecretKey> for PrivateKeySigner {
    fn from(value: K256SecretKey) -> Self {
        Self::from_signing_key(value.into())
    }
}

impl From<&K256SecretKey> for PrivateKeySigner {
    fn from(value: &K256SecretKey) -> Self {
        Self::from_signing_key(value.into())
    }
}

impl FromStr for PrivateKeySigner {
    type Err = LocalSignerError;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        let array = hex::decode_to_array::<_, 32>(src)?;
        Ok(Self::from_slice(&array)?)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use alloy_signer::SignerSync;

    use super::*;
    use crate::PrivateKeySigner;

    #[test]
    fn parse_pk() {
        let s = "6f142508b4eea641e33cb2a0161221105086a84584c74245ca463a49effea30b";
        let _pk: PrivateKeySigner = s.parse().unwrap();
    }

    #[test]
    fn parse_short_key() {
        let s = "6f142508b4eea641e33cb2a0161221105086a84584c74245ca463a49effea3";
        assert!(s.len() < 64);
        let pk = s.parse::<PrivateKeySigner>().unwrap_err();
        match pk {
            LocalSignerError::HexError(hex::FromHexError::InvalidStringLength) => {}
            _ => panic!("Unexpected error"),
        }
    }

    #[test]
    fn signs_msg() {
        let message = "Some data";
        let hash = alloy_primitives::utils::eip191_hash_message(message);
        let key = PrivateKeySigner::random_with(&mut rand_08::thread_rng());
        let address = key.address;

        // sign a message
        let signature = key.sign_message_sync(message.as_bytes()).unwrap();

        // ecrecover via the message will hash internally
        let recovered = signature.recover_address_from_msg(message).unwrap();
        assert_eq!(recovered, address);

        // if provided with a hash, it will skip hashing
        let recovered2 = signature.recover_address_from_prehash(&hash).unwrap();
        assert_eq!(recovered2, address);
    }

    #[test]
    fn key_to_address() {
        let signer: PrivateKeySigner =
            "0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();
        assert_eq!(signer.address, address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf"));

        let signer: PrivateKeySigner =
            "0000000000000000000000000000000000000000000000000000000000000002".parse().unwrap();
        assert_eq!(signer.address, address!("2B5AD5c4795c026514f8317c7a215E218DcCD6cF"));

        let signer: PrivateKeySigner =
            "0000000000000000000000000000000000000000000000000000000000000003".parse().unwrap();
        assert_eq!(signer.address, address!("0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69"));
    }

    #[test]
    fn conversions() {
        let key = b256!("0000000000000000000000000000000000000000000000000000000000000001");

        let signer_b256: PrivateKeySigner = PrivateKeySigner::from_bytes(&key).unwrap();
        assert_eq!(signer_b256.address, address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf"));
        assert_eq!(signer_b256.chain_id, None);
        assert_eq!(signer_b256.credential, SigningKey::from_bytes((&key.0).into()).unwrap());

        let signer_str = PrivateKeySigner::from_str(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap();
        assert_eq!(signer_str.address, signer_b256.address);
        assert_eq!(signer_str.chain_id, signer_b256.chain_id);
        assert_eq!(signer_str.credential, signer_b256.credential);
        assert_eq!(signer_str.to_bytes(), key);
        assert_eq!(signer_str.to_field_bytes(), key.0.into());

        let signer_slice = PrivateKeySigner::from_slice(&key[..]).unwrap();
        assert_eq!(signer_slice.address, signer_b256.address);
        assert_eq!(signer_slice.chain_id, signer_b256.chain_id);
        assert_eq!(signer_slice.credential, signer_b256.credential);
        assert_eq!(signer_slice.to_bytes(), key);
        assert_eq!(signer_slice.to_field_bytes(), key.0.into());

        let signer_field_bytes = PrivateKeySigner::from_field_bytes((&key.0).into()).unwrap();
        assert_eq!(signer_field_bytes.address, signer_b256.address);
        assert_eq!(signer_field_bytes.chain_id, signer_b256.chain_id);
        assert_eq!(signer_field_bytes.credential, signer_b256.credential);
        assert_eq!(signer_field_bytes.to_bytes(), key);
        assert_eq!(signer_field_bytes.to_field_bytes(), key.0.into());
    }

    #[test]
    fn key_from_str() {
        let signer: PrivateKeySigner =
            "0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();

        // Check FromStr and `0x`
        let signer_0x: PrivateKeySigner =
            "0x0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();
        assert_eq!(signer.address, signer_0x.address);
        assert_eq!(signer.chain_id, signer_0x.chain_id);
        assert_eq!(signer.credential, signer_0x.credential);

        // Must fail because of `0z`
        "0z0000000000000000000000000000000000000000000000000000000000000001"
            .parse::<PrivateKeySigner>()
            .unwrap_err();
    }

    #[test]
    fn public_key() {
        let signer: PrivateKeySigner =
            "0x51fde55a7d696da3b318b21e231dec5ff4b33e895f191b2988e122e969b20e90".parse().unwrap();
        assert_eq!(signer.public_key(), B512::from_str("0x2bcb56445551cd344c9be67cfe27652932d7088c17b6c3c8dad622a5c8e8caf4574d68fa12355e7fefbe2377911016124b9284283527dd2ead05c7b6e5585fbd").unwrap());
    }
}
