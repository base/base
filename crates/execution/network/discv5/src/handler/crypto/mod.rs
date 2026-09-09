//! Implementation for generating session keys in the Discv5 protocol.
//! Currently, Diffie-Hellman key agreement is performed with known public key types. Session keys
//! are then derived using the HKDF (SHA2-256) key derivation function.
//!
//! There is no abstraction in this module as the specification explicitly defines a singular
//! encryption and key-derivation algorithms. Future versions may abstract some of these to allow
//! for different algorithms.
use std::convert::TryFrom;

use aes_gcm::{
    Aes128Gcm,
    aead::{Aead, KeyInit, Payload, generic_array::GenericArray},
};
use ecdh::ecdh;
use enr::{
    CombinedKey, CombinedPublicKey, NodeId,
    k256::{
        self,
        ecdsa::{
            Signature,
            signature::{DigestSigner, DigestVerifier},
        },
        sha2::{Digest, Sha256},
    },
};
use hkdf::Hkdf;

use crate::{
    error::Error,
    node_info::NodeContact,
    packet::{ChallengeData, MessageNonce},
};

mod ecdh;
pub use ecdh::ecdh as shared_secret;

const NODE_ID_LENGTH: usize = 32;
const INFO_LENGTH: usize = 26 + 2 * NODE_ID_LENGTH;
const KEY_LENGTH: usize = 16;
const KEY_AGREEMENT_STRING: &str = "discovery v5 key agreement";
const ID_SIGNATURE_TEXT: &str = "discovery v5 identity proof";

type Key = [u8; KEY_LENGTH];

/* Session key generation */

/// Generates session and auth-response keys for a nonce and remote ENR. This currently only
/// supports Secp256k1 signed ENR's. This returns four keys; initiator key, responder key, auth
/// response key and the ephemeral public key.
pub(crate) fn generate_session_keys(
    local_id: &NodeId,
    contact: &NodeContact,
    challenge_data: &ChallengeData,
) -> Result<(Key, Key, Vec<u8>), Error> {
    let (secret, ephem_pk) = {
        match contact.public_key() {
            CombinedPublicKey::Secp256k1(remote_pk) => {
                let ephem_sk = k256::ecdsa::SigningKey::random(&mut rand_08::thread_rng());
                let secret = ecdh(&remote_pk, &ephem_sk);
                let ephem_pk = ephem_sk.verifying_key();
                (secret, ephem_pk.to_sec1_bytes().to_vec())
            }
            CombinedPublicKey::Ed25519(_) => return Err(Error::KeyTypeNotSupported("Ed25519")),
        }
    };

    let (initiator_key, recipient_key) =
        derive_key(&secret, local_id, &contact.node_id(), challenge_data)?;

    Ok((initiator_key, recipient_key, ephem_pk))
}

fn derive_key(
    secret: &[u8],
    first_id: &NodeId,
    second_id: &NodeId,
    challenge_data: &ChallengeData,
) -> Result<(Key, Key), Error> {
    let mut info = [0u8; INFO_LENGTH];
    info[0..26].copy_from_slice(KEY_AGREEMENT_STRING.as_bytes());
    info[26..26 + NODE_ID_LENGTH].copy_from_slice(&first_id.raw());
    info[26 + NODE_ID_LENGTH..].copy_from_slice(&second_id.raw());

    let hk = Hkdf::<Sha256>::new(Some(challenge_data.as_ref()), secret);

    let mut okm = [0u8; 2 * KEY_LENGTH];
    hk.expand(&info, &mut okm).map_err(|_| Error::KeyDerivationFailed)?;

    let mut initiator_key: Key = Default::default();
    let mut recipient_key: Key = Default::default();
    initiator_key.copy_from_slice(&okm[0..KEY_LENGTH]);
    recipient_key.copy_from_slice(&okm[KEY_LENGTH..2 * KEY_LENGTH]);

    Ok((initiator_key, recipient_key))
}

/// Derives the session keys for a public key type that matches the local keypair.
pub(crate) fn derive_keys_from_pubkey(
    local_key: &CombinedKey,
    local_id: &NodeId,
    remote_id: &NodeId,
    challenge_data: &ChallengeData,
    ephem_pubkey: &[u8],
) -> Result<(Key, Key), Error> {
    let secret = {
        match local_key {
            CombinedKey::Secp256k1(key) => {
                // convert remote pubkey into secp256k1 public key
                // the key type should match our own node record
                let remote_pubkey = k256::ecdsa::VerifyingKey::from_sec1_bytes(ephem_pubkey)
                    .map_err(|_| Error::InvalidRemotePublicKey)?;
                ecdh(&remote_pubkey, key)
            }
            CombinedKey::Ed25519(_) => return Err(Error::KeyTypeNotSupported("Ed25519")),
        }
    };

    derive_key(&secret, remote_id, local_id, challenge_data)
}

/* Nonce Signing */

/// Generates a signature of a nonce given a keypair. This prefixes the `NONCE_PREFIX` to the
/// signature.
pub(crate) fn sign_nonce(
    signing_key: &CombinedKey,
    challenge_data: &ChallengeData,
    ephem_pubkey: &[u8],
    dst_id: &NodeId,
) -> Result<Vec<u8>, Error> {
    let signing_message = generate_signing_nonce(challenge_data, ephem_pubkey, dst_id);

    match signing_key {
        CombinedKey::Secp256k1(key) => {
            let message = Sha256::new().chain_update(signing_message);
            let signature: Signature = key
                .try_sign_digest(message)
                .map_err(|e| Error::Error(format!("Failed to sign message: {e}")))?;
            Ok(signature.to_vec())
        }
        CombinedKey::Ed25519(_) => Err(Error::KeyTypeNotSupported("Ed25519")),
    }
}

/// Verifies the authentication header nonce.
pub(crate) fn verify_authentication_nonce(
    remote_pubkey: &CombinedPublicKey,
    remote_ephem_pubkey: &[u8],
    challenge_data: &ChallengeData,
    dst_id: &NodeId,
    sig: &[u8],
) -> bool {
    let signing_nonce = generate_signing_nonce(challenge_data, remote_ephem_pubkey, dst_id);

    match remote_pubkey {
        CombinedPublicKey::Secp256k1(key) => {
            if let Ok(sig) = k256::ecdsa::Signature::try_from(sig) {
                return key.verify_digest(Sha256::new().chain_update(signing_nonce), &sig).is_ok();
            }
            false
        }
        CombinedPublicKey::Ed25519(_) => {
            // key not yet supported
            false
        }
    }
}

/// Builds the signature for a given challenge data.
///
/// This takes the SHA256 hash of the nonce.
fn generate_signing_nonce(
    challenge_data: &ChallengeData,
    ephem_pubkey: &[u8],
    dst_id: &NodeId,
) -> Vec<u8> {
    let mut data = ID_SIGNATURE_TEXT.as_bytes().to_vec();
    data.extend_from_slice(challenge_data.as_ref());
    data.extend_from_slice(ephem_pubkey);
    data.extend_from_slice(dst_id.raw().as_ref());
    data
}

/* Decryption related functions */

/// Decrypt messages that are post-fixed with an authenticated MAC.
pub(crate) fn decrypt_message(
    key: &Key,
    message_nonce: MessageNonce,
    msg: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, Error> {
    if msg.len() < 16 {
        return Err(Error::DecryptionFailed("Message not long enough to contain a MAC".into()));
    }

    let aead = Aes128Gcm::new(GenericArray::from_slice(key));
    let payload = Payload { msg, aad };
    aead.decrypt(GenericArray::from_slice(&message_nonce), payload)
        .map_err(|e| Error::DecryptionFailed(e.to_string()))
}

/* Encryption related functions */

/// A wrapper around the underlying default AES_GCM implementation. This may be abstracted in the
/// future.
pub(crate) fn encrypt_message(
    key: &Key,
    message_nonce: MessageNonce,
    msg: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, Error> {
    let aead = Aes128Gcm::new(GenericArray::from_slice(key));
    let payload = Payload { msg, aad };
    aead.encrypt(GenericArray::from_slice(&message_nonce), payload)
        .map_err(|e| Error::DecryptionFailed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::convert::TryInto;

    use enr::{CombinedKey, Enr, EnrKey};

    use super::*;
    use crate::ProtocolIdentity;

    fn hex_decode(x: &'static str) -> Vec<u8> {
        hex::decode(x).unwrap()
    }

    fn node_key_1() -> CombinedKey {
        CombinedKey::secp256k1_from_bytes(&mut hex_decode(
            "eef77acb6c6a6eebc5b363a475ac583ec7eccdb42b6481424c60f59aa326547f",
        ))
        .unwrap()
    }

    fn node_key_2() -> CombinedKey {
        CombinedKey::secp256k1_from_bytes(&mut hex_decode(
            "66fb62bfbd66b9177a138c1e5cddbe4f7c30c343e94e68df8769459cb1cde628",
        ))
        .unwrap()
    }
    /* This section provides a series of reference tests for the encoding of packets */

    #[test]
    fn ref_test_ecdh() {
        let remote_pubkey =
            hex::decode("039961e4c2356d61bedb83052c115d311acb3a96f5777296dcf297351130266231")
                .unwrap();
        let local_secret_key =
            hex::decode("fb757dc581730490a1d7a00deea65e9b1936924caaea8f44d476014856b68736")
                .unwrap();

        let expected_secret =
            hex::decode("033b11a2a1f214567e1537ce5e509ffd9b21373247f2a3ff6841f4976f53165e7e")
                .unwrap();

        let remote_pk = k256::ecdsa::VerifyingKey::from_sec1_bytes(&remote_pubkey).unwrap();
        let local_sk = k256::ecdsa::SigningKey::from_slice(&local_secret_key).unwrap();

        let secret = ecdh(&remote_pk, &local_sk);
        assert_eq!(secret, expected_secret);
    }

    #[test]
    fn ref_key_derivation() {
        let ephem_key =
            hex::decode("fb757dc581730490a1d7a00deea65e9b1936924caaea8f44d476014856b68736")
                .unwrap();
        let dest_pubkey =
            hex::decode("0317931e6e0840220642f230037d285d122bc59063221ef3226b1f403ddc69ca91")
                .unwrap();

        let remote_pk = k256::ecdsa::VerifyingKey::from_sec1_bytes(&dest_pubkey).unwrap();
        let local_sk = k256::ecdsa::SigningKey::from_slice(&ephem_key).unwrap();

        let secret = ecdh(&remote_pk, &local_sk);

        let first_node_id: NodeId = node_key_1().public().into();
        let second_node_id: NodeId = node_key_2().public().into();

        let challenge_data: ChallengeData = hex::decode("000000000000000000000000000000006469736376350001010102030405060708090a0b0c00180102030405060708090a0b0c0d0e0f100000000000000000").unwrap().as_slice().try_into().unwrap();

        let expected_first_key = hex::decode("dccc82d81bd610f4f76d3ebe97a40571").unwrap();
        let expected_second_key = hex::decode("ac74bb8773749920b0d3a8881c173ec5").unwrap();

        let (first_key, second_key) =
            derive_key(&secret, &first_node_id, &second_node_id, &challenge_data).unwrap();

        assert_eq!(first_key.to_vec(), expected_first_key);
        assert_eq!(second_key.to_vec(), expected_second_key);
    }

    #[test]
    fn ref_nonce_signing() {
        let ephemeral_pubkey =
            hex::decode("039961e4c2356d61bedb83052c115d311acb3a96f5777296dcf297351130266231")
                .unwrap();
        let local_secret_key =
            hex::decode("fb757dc581730490a1d7a00deea65e9b1936924caaea8f44d476014856b68736")
                .unwrap();
        let dst_id: NodeId = node_key_2().public().into();

        println!("{dst_id}");

        let expected_sig = hex::decode("94852a1e2318c4e5e9d422c98eaf19d1d90d876b29cd06ca7cb7546d0fff7b484fe86c09a064fe72bdbef73ba8e9c34df0cd2b53e9d65528c2c7f336d5dfc6e6").unwrap();

        let challenge_data = ChallengeData::try_from(hex::decode("000000000000000000000000000000006469736376350001010102030405060708090a0b0c00180102030405060708090a0b0c0d0e0f100000000000000000").unwrap().as_slice()).unwrap();
        let key = k256::ecdsa::SigningKey::from_slice(&local_secret_key).unwrap();
        let sig = sign_nonce(&key.into(), &challenge_data, &ephemeral_pubkey, &dst_id).unwrap();

        assert_eq!(sig, expected_sig);
    }

    #[test]
    fn ref_encryption() {
        let key_bytes = hex::decode("9f2d77db7004bf8a1a85107ac686990b").unwrap();
        let nonce_bytes = hex::decode("27b5af763c446acd2749fe8e").unwrap();
        let pt = hex::decode("01c20101").unwrap();
        let ad = hex::decode("93a7400fa0d6a694ebc24d5cf570f65d04215b6ac00757875e3f3a5f42107903")
            .unwrap();
        let expected_ciphertext = hex::decode("a5d12a2d94b8ccb3ba55558229867dc13bfa3648").unwrap();

        let mut key = [0u8; 16];
        key.copy_from_slice(&key_bytes);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&nonce_bytes);

        let ciphertext = encrypt_message(&key, nonce, &pt, &ad).unwrap();

        assert_eq!(ciphertext, expected_ciphertext);
    }

    /* This section provides functionality testing */

    #[test]
    fn derive_symmetric_keys() {
        let node1_key = CombinedKey::generate_secp256k1();
        let node2_key = CombinedKey::generate_secp256k1();

        let node1_enr =
            Enr::builder().ip("127.0.0.1".parse().unwrap()).udp4(9000).build(&node1_key).unwrap();
        let node2_enr =
            Enr::builder().ip("127.0.0.1".parse().unwrap()).udp4(9000).build(&node2_key).unwrap();

        let challenge_data = vec![1; 63];
        let challenge_data = ChallengeData::try_from(challenge_data.as_slice()).unwrap();

        let (key1, key2, pk) =
            generate_session_keys(&node1_enr.node_id(), &node2_enr.clone().into(), &challenge_data)
                .unwrap();
        let (key4, key5) = derive_keys_from_pubkey(
            &node2_key,
            &node2_enr.node_id(),
            &node1_enr.node_id(),
            &challenge_data,
            &pk,
        )
        .unwrap();

        assert_eq!(key1, key4);
        assert_eq!(key2, key5);
    }

    #[test]
    fn encrypt_decrypt() {
        // aad
        let aad: [u8; 12] = rand_08::random();
        let msg: Vec<u8> = vec![1, 2, 3, 4, 5, 6];
        let key: Key = rand_08::random();
        let nonce: MessageNonce = rand_08::random();

        let cipher = encrypt_message(&key, nonce, &msg, &aad).unwrap();
        let plain_text = decrypt_message(&key, nonce, &cipher, &aad).unwrap();

        assert_eq!(plain_text, msg);
    }

    #[test]
    fn decrypt_ref_test_ping() {
        let dst_id: NodeId = node_key_2().public().into();
        let encoded_ref_packet = hex::decode("00000000000000000000000000000000088b3d4342774649325f313964a39e55ea96c005ad52be8c7560413a7008f16c9e6d2f43bbea8814a546b7409ce783d34c4f53245d08dab84102ed931f66d1492acb308fa1c6715b9d139b81acbdcc").unwrap();
        let (_packet, auth_data) = crate::packet::Packet::decode(
            &dst_id,
            ProtocolIdentity::default(),
            &encoded_ref_packet,
        )
        .unwrap();

        let ciphertext = hex::decode("b84102ed931f66d1492acb308fa1c6715b9d139b81acbdcc").unwrap();
        let read_key = hex::decode("00000000000000000000000000000000").unwrap();
        let mut key = [0u8; 16];
        key.copy_from_slice(&read_key);
        let byte_nonce = hex::decode("ffffffffffffffffffffffff").unwrap();
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&byte_nonce);

        let message = decrypt_message(&key, nonce, &ciphertext, &auth_data).unwrap();
        dbg!(&message);
        dbg!(hex::encode(&message));
        let rpc = crate::rpc::Message::decode(&message).unwrap();

        println!("{rpc}");
    }
}
