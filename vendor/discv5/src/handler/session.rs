use enr::{CombinedKey, NodeId};
use zeroize::Zeroize;

use super::*;
use crate::{
    ProtocolIdentity,
    node_info::NodeContact,
    packet::{ChallengeData, MESSAGE_NONCE_LENGTH, Packet, PacketHeader, PacketKind},
};

#[derive(Zeroize, PartialEq)]
pub struct Keys {
    /// The encryption key.
    encryption_key: [u8; 16],
    /// The decryption key.
    decryption_key: [u8; 16],
}

/// A Session containing the encryption/decryption keys. These are kept individually for a given
/// node.
#[derive(Debug)]
pub struct Session {
    /// The current keys used to encrypt/decrypt messages.
    keys: Keys,
    /// If a new handshake is being established, the older keys are maintained as race
    /// conditions in the handshake can give different views of which keys are canon.
    /// The key that worked to decrypt our last message (or are freshly established) exist in
    /// `keys` and previous keys are optionally stored in `old_keys`. We attempt to decrypt
    /// messages with `keys` before optionally trying `old_keys`.
    old_keys: Option<Keys>,
    /// If we contacted this node without an ENR, i.e. via a multiaddr, during the session
    /// establishment we request the nodes ENR. Once the ENR is received and verified, this session
    /// becomes established.
    ///
    /// This field holds the request_id associated with the ENR request.
    pub awaiting_enr: Option<RequestId>,
    /// Number of messages sent. Used to ensure the nonce used in message encryption is always
    /// unique.
    counter: u32,
}

impl Session {
    pub fn new(keys: Keys) -> Self {
        Session { keys, old_keys: None, awaiting_enr: None, counter: 0 }
    }

    /// A new session has been established. Update this session based on the new session.
    pub fn update(&mut self, new_session: Session) {
        // Optimistically assume the new keys are canonical.
        self.old_keys = Some(std::mem::replace(&mut self.keys, new_session.keys));
        self.awaiting_enr = new_session.awaiting_enr;
    }

    /// Uses the current `Session` to encrypt a message. Encrypt packets with the current session
    /// key if we are awaiting a response from AuthMessage.
    pub(crate) fn encrypt_message(
        &mut self,
        src_id: NodeId,
        message: &[u8],
        protocol_identity: ProtocolIdentity,
    ) -> Result<Packet, Error> {
        self.counter += 1;

        // If the message nonce length is ever set below 4 bytes this will explode. The packet
        // size constants shouldn't be modified.
        let random_nonce: [u8; MESSAGE_NONCE_LENGTH - 4] = rand_08::random();
        let mut message_nonce: MessageNonce = [0u8; MESSAGE_NONCE_LENGTH];
        message_nonce[..4].copy_from_slice(&self.counter.to_be_bytes());
        message_nonce[4..].copy_from_slice(&random_nonce);

        // the authenticated data is the IV concatenated with the packet header
        let iv: u128 = rand_08::random();
        let header =
            PacketHeader { message_nonce, kind: PacketKind::Message { src_id }, protocol_identity };

        let mut authenticated_data = iv.to_be_bytes().to_vec();
        authenticated_data.extend_from_slice(&header.encode());

        let cipher = crypto::encrypt_message(
            &self.keys.encryption_key,
            message_nonce,
            message,
            &authenticated_data,
        )?;

        // construct a packet from the header and the cipher text
        Ok(Packet { iv, header, message: cipher })
    }

    /// Decrypts an encrypted message. If a Session is already established, the original decryption
    /// keys are tried first, upon failure, the new keys are attempted. If the new keys succeed,
    /// the session keys are updated along with the Session state.
    pub(crate) fn decrypt_message(
        &mut self,
        message_nonce: MessageNonce,
        message: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, Error> {
        // First try with the canonical keys.
        let result_canon =
            crypto::decrypt_message(&self.keys.decryption_key, message_nonce, message, aad);

        // If decryption is fine, nothing more to do.
        if result_canon.is_ok() {
            return result_canon;
        }

        // If these keys did not work, try old_keys
        if let Some(old_keys) = self.old_keys.take() {
            let result =
                crypto::decrypt_message(&old_keys.decryption_key, message_nonce, message, aad);
            if result.is_ok() {
                // rotate the keys
                self.old_keys = Some(std::mem::replace(&mut self.keys, old_keys));
            }
            return result;
        }
        result_canon
    }

    /* Session Helper Functions */

    /// Generates session keys from an authentication header. If the IP of the ENR does not match the
    /// source IP address, we consider this session untrusted. The output returns a boolean which
    /// specifies if the Session is trusted or not.
    pub(crate) fn establish_from_challenge(
        local_key: Arc<RwLock<CombinedKey>>,
        local_id: &NodeId,
        remote_id: &NodeId,
        challenge: Challenge,
        id_nonce_sig: &[u8],
        ephem_pubkey: &[u8],
        enr_record: Option<Enr>,
    ) -> Result<(Session, Enr), Error> {
        // check and verify a potential ENR update

        // Duplicate code here to avoid cloning an ENR
        let remote_public_key = {
            let enr = match (enr_record.as_ref(), challenge.remote_enr.as_ref()) {
                (Some(new_enr), Some(known_enr)) => {
                    if new_enr.seq() > known_enr.seq() {
                        new_enr
                    } else {
                        known_enr
                    }
                }
                (Some(new_enr), None) => new_enr,
                (None, Some(known_enr)) => known_enr,
                (None, None) => {
                    warn!(
                        node = %remote_id,
                        "Peer did not respond with their ENR. Session could not be established",
                    );
                    return Err(Error::SessionNotEstablished);
                }
            };
            enr.public_key()
        };

        // verify the auth header nonce
        if !crypto::verify_authentication_nonce(
            &remote_public_key,
            ephem_pubkey,
            &challenge.data,
            local_id,
            id_nonce_sig,
        ) {
            return Err(Error::InvalidChallengeSignature(Box::new(challenge)));
        }

        // The keys are derived after the message has been verified to prevent potential extra work
        // for invalid messages.

        // generate session keys
        let (decryption_key, encryption_key) = crypto::derive_keys_from_pubkey(
            &local_key.read(),
            local_id,
            remote_id,
            &challenge.data,
            ephem_pubkey,
        )?;

        let keys = Keys { encryption_key, decryption_key };

        // Takes ownership of the provided ENRs - Slightly annoying code duplication, but avoids
        // cloning ENRs
        let session_enr = match (enr_record, challenge.remote_enr) {
            (Some(new_enr), Some(known_enr)) => {
                if new_enr.seq() > known_enr.seq() {
                    new_enr
                } else {
                    known_enr
                }
            }
            (Some(new_enr), None) => new_enr,
            (None, Some(known_enr)) => known_enr,
            (None, None) => unreachable!("Checked in the first match above"),
        };

        Ok((Session::new(keys), session_enr))
    }

    /// Encrypts a message and produces an AuthMessage.
    pub(crate) fn encrypt_with_header(
        remote_contact: &NodeContact,
        local_key: Arc<RwLock<CombinedKey>>,
        updated_enr: Option<Enr>,
        local_node_id: &NodeId,
        protocol_identity: ProtocolIdentity,
        challenge_data: &ChallengeData,
        message: &[u8],
    ) -> Result<(Packet, Session), Error> {
        // generate the session keys
        let (encryption_key, decryption_key, ephem_pubkey) =
            crypto::generate_session_keys(local_node_id, remote_contact, challenge_data)?;

        let keys = Keys { encryption_key, decryption_key };

        // construct the nonce signature
        let sig = crypto::sign_nonce(
            &local_key.read(),
            challenge_data,
            &ephem_pubkey,
            &remote_contact.node_id(),
        )
        .map_err(|_| Error::Custom("Could not sign WHOAREYOU nonce"))?;

        // build an authentication packet
        let message_nonce: MessageNonce = rand_08::random();
        let mut packet = Packet::new_authheader(
            *local_node_id,
            message_nonce,
            protocol_identity,
            sig,
            ephem_pubkey,
            updated_enr,
        );

        // Create the authenticated data for the new packet.

        let mut authenticated_data = packet.iv.to_be_bytes().to_vec();
        authenticated_data.extend_from_slice(&packet.header.encode());

        // encrypt the message
        let message_ciphertext =
            crypto::encrypt_message(&encryption_key, message_nonce, message, &authenticated_data)?;

        packet.message = message_ciphertext;

        let session = Session::new(keys);

        Ok((packet, session))
    }
}

impl core::fmt::Debug for Keys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Keys").finish_non_exhaustive()
    }
}
