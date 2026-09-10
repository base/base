//! This module defines the raw UDP message packets for Discovery v5.1.
//!
//! The [discv5 wire specification](https://github.com/ethereum/devp2p/blob/master/discv5/discv5.md) provides further information on UDP message packets as implemented in this module.
//!
//! A [`Packet`] defines all raw UDP message variants and implements the encoding/decoding
//! logic.
//!
//! Note, that all message encryption/decryption is handled outside of this module.
//!
//! [`Packet`]: enum.Packet.html

use aes::cipher::{KeyIvInit, StreamCipher, generic_array::GenericArray};

use crate::discv5_protocol::{Enr, error::PacketError};

type Aes128Ctr64BE = ctr::Ctr64BE<aes::Aes128>;

use std::convert::TryInto;

use alloy_rlp::Decodable;
use enr::NodeId;
use rand_08::Rng;
use zeroize::Zeroize;

/// The packet IV length (u128).
pub const IV_LENGTH: usize = 16;
/// The length of the static header. (6 byte protocol id, 2 bytes version, 1 byte kind, 12 byte
/// message nonce and a 2 byte authdata-size).
pub const STATIC_HEADER_LENGTH: usize = 23;
/// The message nonce length (in bytes).
pub const MESSAGE_NONCE_LENGTH: usize = 12;
/// The Id nonce length (in bytes).
pub const ID_NONCE_LENGTH: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolIdentity {
    pub protocol_id: [u8; 6],
    pub protocol_version: [u8; 2],
}

impl Default for ProtocolIdentity {
    fn default() -> Self {
        ProtocolIdentity { protocol_id: *b"discv5", protocol_version: 0x0001_u16.to_be_bytes() }
    }
}

pub(crate) const MAX_PACKET_SIZE: usize = 1280;
// The smallest packet must be at least this large
// The 24 is the smallest auth_data that can be sent (it is by a WHOAREYOU packet)
const MIN_PACKET_SIZE: usize = IV_LENGTH + STATIC_HEADER_LENGTH + 24;

/// Message Nonce (12 bytes).
pub type MessageNonce = [u8; MESSAGE_NONCE_LENGTH];
/// The nonce sent in a WHOAREYOU packet.
pub type IdNonce = [u8; ID_NONCE_LENGTH];

// This is the WHOAREYOU authenticated data.
pub struct ChallengeData([u8; 63]);

impl std::fmt::Debug for ChallengeData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl std::convert::TryFrom<&[u8]> for ChallengeData {
    type Error = ();

    fn try_from(data: &[u8]) -> Result<Self, ()> {
        if data.len() != 63 {
            return Err(());
        }
        let mut result = [0; 63];
        result.copy_from_slice(data);
        Ok(ChallengeData(result))
    }
}

impl AsRef<[u8]> for ChallengeData {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Random data unique to the packet.
    pub iv: u128,
    /// Protocol header.
    pub header: PacketHeader,
    /// The message contents itself.
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketHeader {
    /// The nonce of the associated message
    pub message_nonce: MessageNonce,
    /// The protocol identity used
    pub protocol_identity: ProtocolIdentity,
    /// The type of packet this is.
    pub kind: PacketKind,
}

impl PacketHeader {
    // Encodes the header to bytes to be included into the `masked-header` of the Packet Encoding.
    pub fn encode(&self) -> Vec<u8> {
        let auth_data = self.kind.encode();
        let mut buf = Vec::with_capacity(auth_data.len() + STATIC_HEADER_LENGTH);
        buf.extend_from_slice(&self.protocol_identity.protocol_id);
        buf.extend_from_slice(&self.protocol_identity.protocol_version);
        let kind: u8 = (&self.kind).into();
        buf.extend_from_slice(&kind.to_be_bytes());
        buf.extend_from_slice(&self.message_nonce);
        buf.extend_from_slice(&(auth_data.len() as u16).to_be_bytes());
        buf.extend_from_slice(&auth_data);
        buf
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketKind {
    /// An ordinary message.
    Message {
        /// The sending NodeId.
        src_id: NodeId,
    },
    /// A WHOAREYOU packet.
    WhoAreYou {
        /// The ID Nonce to be verified.
        id_nonce: IdNonce,
        /// The local node's current ENR sequence number.
        enr_seq: u64,
    },
    /// A handshake message.
    Handshake {
        /// The sending NodeId
        src_id: NodeId,
        /// Id-nonce signature that matches the WHOAREYOU request.
        id_nonce_sig: Vec<u8>,
        /// The ephemeral public key of the handshake.
        ephem_pubkey: Vec<u8>,
        /// The ENR record of the node if the WHOAREYOU request is out-dated.
        enr_record: Option<Enr>,
    },
}

impl From<&PacketKind> for u8 {
    fn from(kind: &PacketKind) -> Self {
        match kind {
            PacketKind::Message { .. } => 0,
            PacketKind::WhoAreYou { .. } => 1,
            PacketKind::Handshake { .. } => 2,
        }
    }
}

impl PacketKind {
    /// Encodes the packet type into its corresponding auth_data.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            PacketKind::Message { src_id } => src_id.raw().to_vec(),
            PacketKind::WhoAreYou { id_nonce, enr_seq } => {
                let mut auth_data = Vec::with_capacity(24);
                auth_data.extend_from_slice(id_nonce);
                auth_data.extend_from_slice(&enr_seq.to_be_bytes());
                debug_assert_eq!(auth_data.len(), 24);
                auth_data
            }
            PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record } => {
                let sig_size = id_nonce_sig.len();
                let pubkey_size = ephem_pubkey.len();
                let node_record = enr_record.as_ref().map(alloy_rlp::encode);
                let expected_len = 34
                    + sig_size
                    + pubkey_size
                    + node_record.as_ref().map(|x| x.len()).unwrap_or_default();

                let mut auth_data = Vec::with_capacity(expected_len);
                auth_data.extend_from_slice(&src_id.raw());
                auth_data.extend_from_slice(&(sig_size as u8).to_be_bytes());
                auth_data.extend_from_slice(&(pubkey_size as u8).to_be_bytes());
                auth_data.extend_from_slice(id_nonce_sig);
                auth_data.extend_from_slice(ephem_pubkey);
                if let Some(node_record) = node_record {
                    auth_data.extend_from_slice(&node_record);
                }
                debug_assert_eq!(auth_data.len(), expected_len);
                auth_data
            }
        }
    }

    pub fn is_whoareyou(&self) -> bool {
        matches!(self, PacketKind::WhoAreYou { .. })
    }

    /// Decodes auth data, given the kind byte.
    pub fn decode(kind: u8, auth_data: &[u8]) -> Result<Self, PacketError> {
        match kind {
            0 => {
                // Decoding a message packet
                // This should only contain a 32 byte NodeId.
                if auth_data.len() != 32 {
                    return Err(PacketError::InvalidAuthDataSize);
                }

                let src_id = NodeId::parse(auth_data).map_err(|_| PacketError::InvalidNodeId)?;
                Ok(PacketKind::Message { src_id })
            }
            1 => {
                // Decoding a WHOAREYOU packet authdata
                // This must be 24 bytes long.
                if auth_data.len() != 24 {
                    return Err(PacketError::InvalidAuthDataSize);
                }
                let id_nonce: IdNonce = auth_data[..ID_NONCE_LENGTH]
                    .try_into()
                    .expect("ID_NONCE_LENGTH must be the correct size");
                let enr_seq = u64::from_be_bytes(
                    auth_data[ID_NONCE_LENGTH..]
                        .try_into()
                        .expect("The length of the authdata must be 52 bytes"),
                );

                Ok(PacketKind::WhoAreYou { id_nonce, enr_seq })
            }
            2 => {
                // Decoding a Handshake packet
                // Start by decoding the header
                // Length must contain 2 bytes of lengths and the src id (32 bytes)
                if auth_data.len() < 34 {
                    // The auth_data header is too short
                    return Err(PacketError::InvalidAuthDataSize);
                }

                // decode the src_id
                let src_id =
                    NodeId::parse(&auth_data[..32]).map_err(|_| PacketError::InvalidNodeId)?;

                // decode the lengths
                let sig_size = auth_data[32] as usize;
                let eph_key_size = auth_data[32 + 1] as usize;

                let total_size = sig_size + eph_key_size;

                // verify the auth data length
                if auth_data.len() < 34 + total_size {
                    return Err(PacketError::InvalidAuthDataSize);
                }

                let remaining_data = &auth_data[32 + 2..];

                let id_nonce_sig = remaining_data[0..sig_size].to_vec();
                let ephem_pubkey = remaining_data[sig_size..total_size].to_vec();

                let enr_record = if remaining_data.len() > total_size {
                    Some(
                        <Enr>::decode(&mut &remaining_data[total_size..])
                            .map_err(PacketError::InvalidEnr)?,
                    )
                } else {
                    None
                };

                Ok(PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record })
            }
            _ => Err(PacketError::UnknownPacket),
        }
    }
}

/// The implementation of creating, encoding and decoding raw packets in the discv5.1 system.
//
// NOTE: We perform the encryption and decryption when we are encoding/decoding as this is
// performed in its own task in practice. The Handler can create the messages without the overhead
// of encryption/decryption and send them off to the send/recv tasks to perform the
// encryption/decryption.
impl Packet {
    /// Creates an ordinary message packet.
    pub fn new_message(
        src_id: NodeId,
        message_nonce: MessageNonce,
        protocol_identity: ProtocolIdentity,
        ciphertext: Vec<u8>,
    ) -> Self {
        let iv: u128 = rand_08::random();

        let header =
            PacketHeader { message_nonce, protocol_identity, kind: PacketKind::Message { src_id } };

        Packet { iv, header, message: ciphertext }
    }

    pub fn new_whoareyou(
        request_nonce: MessageNonce,
        id_nonce: IdNonce,
        protocol_identity: ProtocolIdentity,
        enr_seq: u64,
    ) -> Self {
        let iv: u128 = rand_08::random();

        let header = PacketHeader {
            message_nonce: request_nonce,
            protocol_identity,
            kind: PacketKind::WhoAreYou { id_nonce, enr_seq },
        };

        Packet { iv, header, message: Vec::new() }
    }

    pub fn new_authheader(
        src_id: NodeId,
        message_nonce: MessageNonce,
        protocol_identity: ProtocolIdentity,
        id_nonce_sig: Vec<u8>,
        ephem_pubkey: Vec<u8>,
        enr_record: Option<Enr>,
    ) -> Self {
        let iv: u128 = rand_08::random();

        let header = PacketHeader {
            message_nonce,
            protocol_identity,
            kind: PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record },
        };

        Packet { iv, header, message: Vec::new() }
    }

    /// Generates a Packet::Random given a `tag`.
    pub fn new_random(
        src_id: &NodeId,
        protocol_identity: ProtocolIdentity,
    ) -> Result<Self, &'static str> {
        let mut ciphertext = [0u8; 44];
        rand_08::thread_rng().try_fill(&mut ciphertext[..]).map_err(|_| "PRNG failed")?;

        let message_nonce: MessageNonce = rand_08::random();

        Ok(Self::new_message(*src_id, message_nonce, protocol_identity, ciphertext.to_vec()))
    }

    /// Returns true if the packet is a WHOAREYOU packet.
    pub fn is_whoareyou(&self) -> bool {
        match &self.header.kind {
            PacketKind::WhoAreYou { .. } => true,
            PacketKind::Message { .. } | PacketKind::Handshake { .. } => false,
        }
    }

    /// Non-challenge (WHOAREYOU) packets contain the src_id of the node. This function returns the
    /// src_id in this case.
    pub fn src_id(&self) -> Option<NodeId> {
        match self.header.kind {
            PacketKind::Message { src_id } => Some(src_id),
            PacketKind::WhoAreYou { .. } => None,
            PacketKind::Handshake { src_id, .. } => Some(src_id),
        }
    }

    /// Returns the message nonce if one exists.
    pub fn message_nonce(&self) -> &MessageNonce {
        &self.header.message_nonce
    }

    /// Generates the authenticated data for this packet.
    pub fn authenticated_data(&self) -> Vec<u8> {
        let mut authenticated_data = self.iv.to_be_bytes().to_vec();
        authenticated_data.extend_from_slice(&self.header.encode());
        authenticated_data
    }

    /// Encodes a packet to bytes and performs the AES-CTR encryption.
    pub fn encode(self, dst_id: &NodeId) -> Vec<u8> {
        let header = self.encrypt_header(dst_id);
        let mut buf = Vec::with_capacity(IV_LENGTH + header.len() + self.message.len());
        buf.extend_from_slice(&self.iv.to_be_bytes());
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&self.message);
        buf
    }

    /// Creates the masked header of a packet performing the required AES-CTR encryption.
    fn encrypt_header(&self, dst_id: &NodeId) -> Vec<u8> {
        let mut header_bytes = self.header.encode();

        /* Encryption is done inline
         *
         * This was split into its own library, but brought back to allow re-use of the cipher when
         * performing decryption
         */
        let mut key = GenericArray::clone_from_slice(&dst_id.raw()[..16]);
        let mut nonce = GenericArray::clone_from_slice(&self.iv.to_be_bytes());

        let mut cipher = Aes128Ctr64BE::new(&key, &nonce);
        cipher.apply_keystream(&mut header_bytes);
        key.zeroize();
        nonce.zeroize();
        header_bytes
    }

    /// Decodes a packet (data) given our local source id (src_key).
    ///
    /// This also returns the authenticated data for further decryption in the handler.
    pub fn decode(
        src_id: &NodeId,
        protocol_identity: ProtocolIdentity,
        data: &[u8],
    ) -> Result<(Self, Vec<u8>), PacketError> {
        if data.len() > MAX_PACKET_SIZE {
            return Err(PacketError::TooLarge);
        }
        if data.len() < MIN_PACKET_SIZE {
            return Err(PacketError::TooSmall);
        }

        // attempt to decrypt the static header
        let iv = data[..IV_LENGTH].to_vec();

        /* Decryption is done inline
         *
         * This was split into its own library, but brought back to allow re-use of the cipher when
         * performing the decryption
         */
        let key = GenericArray::clone_from_slice(&src_id.raw()[..16]);
        let nonce = GenericArray::clone_from_slice(&iv);
        let mut cipher = Aes128Ctr64BE::new(&key, &nonce);

        // Take the static header content
        let mut static_header = data[IV_LENGTH..IV_LENGTH + STATIC_HEADER_LENGTH].to_vec();
        cipher.apply_keystream(&mut static_header);

        // double check the size
        if static_header.len() != STATIC_HEADER_LENGTH {
            return Err(PacketError::HeaderLengthInvalid(static_header.len()));
        }

        // Check the protocol id
        if static_header[..6] != protocol_identity.protocol_id {
            return Err(PacketError::HeaderDecryptionFailed);
        }

        let version_bytes = &static_header[6..8];
        // Check the version matches
        if version_bytes != protocol_identity.protocol_version {
            let version =
                u16::from_be_bytes(version_bytes.try_into().expect("Must be correct size"));
            return Err(PacketError::InvalidVersion(version));
        }

        let flag = static_header[8];

        // Obtain the message nonce
        let message_nonce: MessageNonce =
            static_header[9..9 + MESSAGE_NONCE_LENGTH].try_into().expect("Must be correct size");

        // The decryption was successful, decrypt the remaining header
        let auth_data_size = u16::from_be_bytes(
            static_header[STATIC_HEADER_LENGTH - 2..]
                .try_into()
                .expect("Can only be 2 bytes in size"),
        );

        let remaining_data = data[IV_LENGTH + STATIC_HEADER_LENGTH..].to_vec();
        if auth_data_size as usize > remaining_data.len() {
            return Err(PacketError::InvalidAuthDataSize);
        }

        let mut auth_data = data[IV_LENGTH + STATIC_HEADER_LENGTH
            ..IV_LENGTH + STATIC_HEADER_LENGTH + auth_data_size as usize]
            .to_vec();
        cipher.apply_keystream(&mut auth_data);

        let kind = PacketKind::decode(flag, &auth_data)?;

        let header = PacketHeader { message_nonce, protocol_identity, kind };

        // Any remaining bytes are message data
        let message = data[IV_LENGTH + STATIC_HEADER_LENGTH + auth_data_size as usize..].to_vec();

        if !message.is_empty() && header.kind.is_whoareyou() {
            // do not allow extra bytes being sent in WHOAREYOU messages
            return Err(PacketError::UnknownPacket);
        }

        // build the authenticated data
        let mut authenticated_data = iv.to_vec();
        authenticated_data.extend_from_slice(&static_header);
        authenticated_data.extend_from_slice(&auth_data);

        let packet = Packet {
            iv: u128::from_be_bytes(iv[..].try_into().expect("IV_LENGTH must be 16 bytes")),
            header,
            message,
        };

        Ok((packet, authenticated_data))
    }
}

impl std::fmt::Display for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Packet {{ iv: {}, header: {}, message {} }}",
            hex::encode(self.iv.to_be_bytes()),
            self.header,
            hex::encode(&self.message)
        )
    }
}

impl std::fmt::Display for PacketHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PacketHeader {{ message_nonce: {}, kind: {} }}",
            hex::encode(self.message_nonce),
            self.kind
        )
    }
}

impl std::fmt::Display for PacketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketKind::Message { src_id } => write!(f, "Message {{ src_id: {src_id} }}"),
            PacketKind::WhoAreYou { id_nonce, enr_seq } => write!(
                f,
                "WhoAreYou {{ id_nonce: {}, enr_seq: {} }}",
                hex::encode(id_nonce),
                enr_seq
            ),
            PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record } => write!(
                f,
                "Handshake {{ src_id : {}, id_nonce_sig: {}, ephem_pubkey: {}, enr_record {:?}",
                hex::encode(src_id.raw()),
                hex::encode(id_nonce_sig),
                hex::encode(ephem_pubkey),
                enr_record
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use enr::{CombinedKey, EnrKey};

    use super::*;

    fn init_log() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    }

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

    #[test]
    fn packet_encode_random() {
        init_log();
        let node_id_a: NodeId = node_key_1().public().into();
        let node_id_b: NodeId = node_key_2().public().into();

        let expected_result = hex::decode("0000000000000000000000000000000b4f3ab1857252f96f758330a846b5d3d4a954d738dfcd6d1ed118ecc1d54f9b20fbf2be28db87805b23193e03c455d73d63ac71dfa91ffa010101010101010101010101").unwrap();
        let iv = 11u128;
        let message_nonce = [12u8; MESSAGE_NONCE_LENGTH];
        let header = PacketHeader {
            message_nonce,
            protocol_identity: Default::default(),
            kind: PacketKind::Message { src_id: node_id_a },
        };
        let message = [1u8; 12].to_vec();
        let packet = Packet { iv, header, message };

        let encoded = packet.encode(&node_id_b);
        dbg!(hex::encode(&encoded));
        assert_eq!(expected_result, encoded);
    }

    #[test]
    fn packet_ref_test_encode_whoareyou() {
        init_log();
        // reference input
        let dst_id: NodeId = node_key_2().public().into();
        let request_nonce: MessageNonce =
            hex_decode("0102030405060708090a0b0c")[..].try_into().unwrap();
        let id_nonce: IdNonce =
            hex_decode("0102030405060708090a0b0c0d0e0f10")[..].try_into().unwrap();
        let enr_seq = 0u64;
        let iv = 0u128;

        // expected hex output
        let expected_output = hex::decode("00000000000000000000000000000000088b3d434277464933a1ccc59f5967ad1d6035f15e528627dde75cd68292f9e6c27d6b66c8100a873fcbaed4e16b8d").unwrap();

        let header = PacketHeader {
            message_nonce: request_nonce,
            protocol_identity: Default::default(),
            kind: PacketKind::WhoAreYou { id_nonce, enr_seq },
        };

        let packet = Packet { iv, header, message: Vec::new() };

        assert_eq!(packet.encode(&dst_id), expected_output);
    }

    #[test]
    fn packet_encode_handshake() {
        init_log();
        // reference input
        let src_id = NodeId::parse(&[3; 32]).unwrap();
        let dst_id = NodeId::parse(&[4; 32]).unwrap();
        let message_nonce: MessageNonce = [52u8; MESSAGE_NONCE_LENGTH];
        let id_nonce_sig = vec![5u8; 64];
        let ephem_pubkey = vec![6u8; 33];
        let enr_record = None;
        let iv = 0u128;

        let expected_output = hex::decode("0000000000000000000000000000000035a14bcdb844ae25f36070f07e0b25e765ed72b4d69c99d5fe5a8d438a4b5b518dfead9d80200875c23e31d0acda6f1b2a6124a70e3dc1f2b8b0770f24d8da18605ff3f5b60b090c61515093a88ef4c02186f7d1b5c9a88fdb8cfae239f13e451758751561b439d8044e27cecdf646f2aa1c9ecbd5faf37eb67a4f6337f4b2a885391e631f72deb808c63bf0b0faed23d7117f7a2e1f98c28bd0").unwrap();

        let header = PacketHeader {
            message_nonce,
            kind: PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record },
            protocol_identity: ProtocolIdentity::default(),
        };

        let packet = Packet { iv, header, message: Vec::new() };
        let encoded = packet.encode(&dst_id);
        assert_eq!(encoded, expected_output);
    }

    #[test]
    fn packet_encode_handshake_enr() {
        // reference input
        let node_key_1 = node_key_1();
        let src_id: NodeId = node_key_1.public().into();
        let dst_id = NodeId::parse(&[4; 32]).unwrap();
        let message_nonce: MessageNonce = [52u8; MESSAGE_NONCE_LENGTH];
        let id_nonce_sig = vec![5u8; 64];
        let ephem_pubkey = vec![6u8; 33];
        let enr_record: Option<Enr> = Some("enr:-IS4QHXuNmr1vGEGVGDcy_sG2BZ7a3A7mbKS812BK_9rToQiF1Lfknsi5o0xKLnGJbTzBssJCzMcIj8SOiu1O9dnfZEBgmlkgnY0gmlwhH8AAAGJc2VjcDI1NmsxoQMT0UIR4Ch7I2GhYViQqbUhIIBUbQoleuTP-Wz1NJksuYN0Y3CCIyg".parse().unwrap());
        let iv = 0u128;

        let expected_output = hex::decode("0000000000000000000000000000000035a14bcdb844ae25f36070f07e0b25e765ed72b4d69d137c57dd97a97dd558d1d8e6e6b6fed699e55bb02b47d25562e0a6486ff2aba179f2b8b0770f24d8da18605ff3f5b60b090c61515093a88ef4c02186f7d1b5c9a88fdb8cfae239f13e451758751561b439d8044e27cecdf646f2aa1c9ecbd5faf37eb67a4f6337f4b2a885391e631f72deb808c63bf0b0faed23d7117f7a2e1f98c28bd0e908ce8b51cc89e592ed2efa671b8efd49e1ce8fd567fdb06ed308267d31f6bd75827812d21e8aa5a6c025e69b67faea57a15c1c9324d16938c4ebe71dba0bd5d7b00bb6de3e846ed37ef13a9d2e271f25233f5d97bbb026223dbe6595210f6a11cbee54589a0c0c20c7bb7c4c5bea46553480e1b7d4e83b2dd8305aac3b15fd9b1a1e13fda0").unwrap();

        let header = PacketHeader {
            message_nonce,
            kind: PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record },
            protocol_identity: ProtocolIdentity::default(),
        };

        let packet = Packet { iv, header, message: Vec::new() };
        let encoded = packet.encode(&dst_id);
        assert_eq!(encoded, expected_output);
    }

    #[test]
    fn packet_ref_test_encode_message() {
        // reference input
        let src_id: NodeId = node_key_1().public().into();
        let dst_id: NodeId = node_key_2().public().into();
        let iv = 0u128;

        let message_nonce: MessageNonce = [52u8; MESSAGE_NONCE_LENGTH];
        let header = PacketHeader {
            message_nonce,
            kind: PacketKind::Message { src_id },
            protocol_identity: ProtocolIdentity::default(),
        };
        let ciphertext = vec![23; 12];

        let expected_output = hex::decode("00000000000000000000000000000000088b3d43427746493294faf2af68559e215d0bce6652be8c7560413a7008f16c9e6d2f43bbea8814a546b7409ce783d34c4f53245d08da171717171717171717171717").unwrap();

        let packet = Packet { iv, header, message: ciphertext };
        let encoded = packet.encode(&dst_id);
        assert_eq!(encoded, expected_output);
    }

    /* This section provides functionality testing of the packets */
    #[test]
    fn packet_encode_decode_random() {
        let src_id: NodeId = node_key_1().public().into();
        let dst_id: NodeId = node_key_2().public().into();

        let packet = Packet::new_random(&src_id, ProtocolIdentity::default()).unwrap();

        let encoded_packet = packet.clone().encode(&dst_id);
        let (decoded_packet, _authenticated_data) =
            Packet::decode(&dst_id, ProtocolIdentity::default(), &encoded_packet).unwrap();

        assert_eq!(decoded_packet, packet);
    }

    #[test]
    fn packet_encode_decode_whoareyou() {
        let dst_id: NodeId = node_key_2().public().into();

        let message_nonce: MessageNonce = rand_08::random();
        let id_nonce: IdNonce = rand_08::random();
        let enr_seq: u64 = rand_08::random();

        let packet =
            Packet::new_whoareyou(message_nonce, id_nonce, ProtocolIdentity::default(), enr_seq);

        let encoded_packet = packet.clone().encode(&dst_id);
        let (decoded_packet, _authenticated_data) =
            Packet::decode(&dst_id, ProtocolIdentity::default(), &encoded_packet).unwrap();

        assert_eq!(decoded_packet, packet);
    }

    #[test]
    fn encode_decode_auth_packet() {
        let src_id: NodeId = node_key_1().public().into();
        let dst_id: NodeId = node_key_2().public().into();

        let message_nonce: MessageNonce = rand_08::random();
        let id_nonce_sig = vec![13; 64];
        let pubkey = vec![11; 33];
        let enr_record = None;

        let packet = Packet::new_authheader(
            src_id,
            message_nonce,
            ProtocolIdentity::default(),
            pubkey,
            id_nonce_sig,
            enr_record,
        );

        let encoded_packet = packet.clone().encode(&dst_id);
        let (decoded_packet, _authenticated_data) =
            Packet::decode(&dst_id, ProtocolIdentity::default(), &encoded_packet).unwrap();

        assert_eq!(decoded_packet, packet);
    }

    #[test]
    fn packet_decode_ref_ping() {
        let src_id: NodeId = node_key_1().public().into();
        let dst_id: NodeId = node_key_2().public().into();
        let message_nonce: MessageNonce =
            hex_decode("ffffffffffffffffffffffff")[..].try_into().unwrap();
        let iv = 0u128;

        let header = PacketHeader {
            message_nonce,
            kind: PacketKind::Message { src_id },
            protocol_identity: ProtocolIdentity::default(),
        };
        let ciphertext = hex_decode("b84102ed931f66d1492acb308fa1c6715b9d139b81acbdcc");
        let expected_packet = Packet { iv, header, message: ciphertext };

        let encoded_ref_packet = hex::decode("00000000000000000000000000000000088b3d4342774649325f313964a39e55ea96c005ad52be8c7560413a7008f16c9e6d2f43bbea8814a546b7409ce783d34c4f53245d08dab84102ed931f66d1492acb308fa1c6715b9d139b81acbdcc").unwrap();

        let (packet, _auth_data) =
            Packet::decode(&dst_id, ProtocolIdentity::default(), &encoded_ref_packet).unwrap();
        assert_eq!(packet, expected_packet);
    }

    #[test]
    fn packet_decode_ref_ping_handshake() {
        let src_id: NodeId = node_key_1().public().into();
        let dst_id: NodeId = node_key_2().public().into();
        let message_nonce: MessageNonce =
            hex_decode("ffffffffffffffffffffffff")[..].try_into().unwrap();
        let id_nonce_sig = hex_decode(
            "c0a04b36f276172afc66a62848eb0769800c670c4edbefab8f26785e7fda6b56506a3f27ca72a75b106edd392a2cbf8a69272f5c1785c36d1de9d98a0894b2db",
        );
        let ephem_pubkey =
            hex_decode("039a003ba6517b473fa0cd74aefe99dadfdb34627f90fec6362df85803908f53a5");
        let enr_record = None;
        let iv = 0u128;

        let header = PacketHeader {
            message_nonce,
            kind: PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record },
            protocol_identity: ProtocolIdentity::default(),
        };

        let message = hex_decode("f1eadf5f0f4126b79336671cbcf7a885b1f8bd2a5d839cf8");
        let expected_packet = Packet { iv, header, message };

        let decoded_ref_packet = hex::decode("00000000000000000000000000000000088b3d4342774649305f313964a39e55ea96c005ad521d8c7560413a7008f16c9e6d2f43bbea8814a546b7409ce783d34c4f53245d08da4bb252012b2cba3f4f374a90a75cff91f142fa9be3e0a5f3ef268ccb9065aeecfd67a999e7fdc137e062b2ec4a0eb92947f0d9a74bfbf44dfba776b21301f8b65efd5796706adff216ab862a9186875f9494150c4ae06fa4d1f0396c93f215fa4ef524f1eadf5f0f4126b79336671cbcf7a885b1f8bd2a5d839cf8").unwrap();

        let (packet, _auth_data) =
            Packet::decode(&dst_id, ProtocolIdentity::default(), &decoded_ref_packet).unwrap();
        assert_eq!(packet, expected_packet);
    }

    #[test]
    fn packet_decode_ref_ping_handshake_enr() {
        let src_id: NodeId = node_key_1().public().into();
        let dst_id: NodeId = node_key_2().public().into();
        let message_nonce: MessageNonce =
            hex_decode("ffffffffffffffffffffffff")[..].try_into().unwrap();
        let id_nonce_sig = hex_decode(
            "a439e69918e3f53f555d8ca4838fbe8abeab56aa55b056a2ac4d49c157ee719240a93f56c9fccfe7742722a92b3f2dfa27a5452f5aca8adeeab8c4d5d87df555",
        );
        let ephem_pubkey =
            hex_decode("039a003ba6517b473fa0cd74aefe99dadfdb34627f90fec6362df85803908f53a5");
        let enr_record = Some("enr:-H24QBfhsHORjaMtZAZCx2LA4ngWmOSXH4qzmnd0atrYPwHnb_yHTFkkgIu-fFCJCILCuKASh6CwgxLR1ToX1Rf16ycBgmlkgnY0gmlwhH8AAAGJc2VjcDI1NmsxoQMT0UIR4Ch7I2GhYViQqbUhIIBUbQoleuTP-Wz1NJksuQ".parse::<Enr>().unwrap());
        let iv = 0u128;

        let header = PacketHeader {
            message_nonce,
            kind: PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record },
            protocol_identity: ProtocolIdentity::default(),
        };

        let message = hex_decode("08d65093ccab5aa596a34d7511401987662d8cf62b139471");
        let expected_packet = Packet { iv, header, message };

        let encoded_ref_packet = hex::decode("00000000000000000000000000000000088b3d4342774649305f313964a39e55ea96c005ad539c8c7560413a7008f16c9e6d2f43bbea8814a546b7409ce783d34c4f53245d08da4bb23698868350aaad22e3ab8dd034f548a1c43cd246be98562fafa0a1fa86d8e7a3b95ae78cc2b988ded6a5b59eb83ad58097252188b902b21481e30e5e285f19735796706adff216ab862a9186875f9494150c4ae06fa4d1f0396c93f215fa4ef524e0ed04c3c21e39b1868e1ca8105e585ec17315e755e6cfc4dd6cb7fd8e1a1f55e49b4b5eb024221482105346f3c82b15fdaae36a3bb12a494683b4a3c7f2ae41306252fed84785e2bbff3b022812d0882f06978df84a80d443972213342d04b9048fc3b1d5fcb1df0f822152eced6da4d3f6df27e70e4539717307a0208cd208d65093ccab5aa596a34d7511401987662d8cf62b139471").unwrap();

        let (packet, _auth_data) =
            Packet::decode(&dst_id, ProtocolIdentity::default(), &encoded_ref_packet).unwrap();
        assert_eq!(packet, expected_packet);
    }

    #[test]
    fn packet_decode_invalid_packet_size() {
        let src_id: NodeId = node_key_1().public().into();

        let data = [0; MAX_PACKET_SIZE + 1];
        let result = Packet::decode(&src_id, ProtocolIdentity::default(), &data);
        assert_eq!(result, Err(PacketError::TooLarge));

        let data = [0; MIN_PACKET_SIZE - 1];
        let result = Packet::decode(&src_id, ProtocolIdentity::default(), &data);
        assert_eq!(result, Err(PacketError::TooSmall));
    }
}
