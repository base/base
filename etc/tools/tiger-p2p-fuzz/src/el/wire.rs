//! The harness's own `RLPx` initiator: a deterministic EIP-8 ECIES handshake and a frame codec
//! that writes any header and body, so the decode targets and the scripted live peer control
//! every byte they send. Written from the devp2p spec (rlpx.md), independently of both
//! implementations under test.

use aes::cipher::{BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use alloy_primitives::{B256, Keccak256, keccak256};
use hmac::Mac;
use secp256k1::{Message, PublicKey, SECP256K1, SecretKey, ecdh};
use sha2::{Digest, Sha256};

use super::{
    rlp::{Item, Node, b, int, l},
    rng::Rng,
};

/// ECIES overhead: ephemeral key (65), IV (16), HMAC tag (32).
pub(crate) const ECIES_OVERHEAD: usize = 65 + 16 + 32;

/// The 64-byte node id of a public key.
pub(crate) fn node_id(public: &PublicKey) -> [u8; 64] {
    public.serialize_uncompressed()[1..].try_into().unwrap()
}

/// Parses a 64-byte node id.
pub(crate) fn public_of_id(id: &[u8]) -> Option<PublicKey> {
    let mut bytes = [4u8; 65];
    bytes[1..].copy_from_slice(id.get(..64)?);
    PublicKey::from_slice(&bytes).ok()
}

/// A secret key from the generator (retrying the negligible invalid draws).
pub(crate) fn secret(rng: &mut Rng) -> SecretKey {
    loop {
        if let Ok(key) = SecretKey::from_byte_array(&rng.array()) {
            return key;
        }
    }
}

fn ecdh_x(public: &PublicKey, secret: &SecretKey) -> [u8; 32] {
    ecdh::shared_secret_point(public, secret)[..32].try_into().unwrap()
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <hmac::Hmac<Sha256> as Mac>::new_from_slice(key).unwrap();
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// AES-128-CTR with the spec's 128-bit big-endian counter (geth does the same).
fn aes128_ctr(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    ctr::Ctr128BE::<aes::Aes128>::new(key.into(), iv.into()).apply_keystream(data);
}

/// ECIES-encrypts `plain` to `remote` (`R || iv || c || tag`), `MACing` `auth_data`.
pub(crate) fn ecies_encrypt(
    rng: &mut Rng,
    remote: &PublicKey,
    plain: &[u8],
    auth_data: &[u8],
    iv: [u8; 16],
) -> Vec<u8> {
    let ephemeral = secret(rng);
    let key = sha256(&[&1u32.to_be_bytes(), &ecdh_x(remote, &ephemeral)]);
    let enc_key: [u8; 16] = key[..16].try_into().unwrap();
    let mac_key = sha256(&[&key[16..]]);
    let mut out =
        PublicKey::from_secret_key(SECP256K1, &ephemeral).serialize_uncompressed().to_vec();
    out.extend_from_slice(&iv);
    let start = out.len();
    out.extend_from_slice(plain);
    aes128_ctr(&enc_key, &iv, &mut out[start..]);
    let tag = hmac_sha256(&mac_key, &[&iv, &out[start..], auth_data]);
    out.extend_from_slice(&tag);
    out
}

/// Opens an EIP-8 packet (size prefix included) addressed to `secret`.
pub(crate) fn open_packet(secret: &SecretKey, packet: &[u8]) -> Option<Vec<u8>> {
    if packet.len() < 2 + ECIES_OVERHEAD {
        return None;
    }
    let (prefix, data) = packet.split_at(2);
    let remote = PublicKey::from_slice(&data[..65]).ok()?;
    let iv: [u8; 16] = data[65..81].try_into().unwrap();
    let (cipher, tag) = data[81..].split_at(data.len() - 81 - 32);
    let key = sha256(&[&1u32.to_be_bytes(), &ecdh_x(&remote, secret)]);
    let mac_key = sha256(&[&key[16..]]);
    if hmac_sha256(&mac_key, &[&iv, cipher, prefix]) != tag {
        return None;
    }
    let mut plain = cipher.to_vec();
    aes128_ctr(key[..16].try_into().unwrap(), &iv, &mut plain);
    Some(plain)
}

/// How to frame an EIP-8 handshake packet around a body.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seal {
    /// Random padding bytes appended to the body (EIP-8 asks for 100 or more).
    pub(crate) padding: usize,
    /// Added to the size prefix after encryption (the MAC covers the declared value).
    pub(crate) prefix_delta: i32,
    /// The IV; `None` draws a random one.
    pub(crate) iv: Option<[u8; 16]>,
}

impl Default for Seal {
    fn default() -> Self {
        Self { padding: 100, prefix_delta: 0, iv: None }
    }
}

/// Seals `body` into an EIP-8 packet for `remote`.
pub(crate) fn seal_packet(rng: &mut Rng, remote: &PublicKey, body: &[u8], seal: Seal) -> Vec<u8> {
    let mut plain = body.to_vec();
    plain.extend(rng.bytes(seal.padding));
    let size = ((plain.len() + ECIES_OVERHEAD) as i32 + seal.prefix_delta).clamp(0, 0xffff) as u16;
    let iv = seal.iv.unwrap_or_else(|| rng.array());
    let mut packet = size.to_be_bytes().to_vec();
    packet.extend(ecies_encrypt(rng, remote, &plain, &size.to_be_bytes(), iv));
    packet
}

/// The initiator's handshake state and the auth body it sends.
#[derive(Debug, Clone)]
pub(crate) struct Handshake {
    /// The initiator's static key.
    pub(crate) static_key: SecretKey,
    /// The ephemeral key the auth signature commits to.
    pub(crate) ephemeral: SecretKey,
    /// The initiator nonce.
    pub(crate) nonce: B256,
}

impl Handshake {
    /// A fresh handshake from the generator.
    pub(crate) fn new(rng: &mut Rng, static_key: SecretKey) -> Self {
        Self { static_key, ephemeral: secret(rng), nonce: B256::from(rng.array::<32>()) }
    }

    /// The auth body `[sig, initiator-id, nonce, version]` as a tree, so it can be mutated.
    pub(crate) fn auth_body(&self, remote: &PublicKey, version: u64) -> Node {
        let shared = ecdh_x(remote, &self.static_key);
        let mut digest = [0u8; 32];
        for (i, d) in digest.iter_mut().enumerate() {
            *d = shared[i] ^ self.nonce[i];
        }
        let sig = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(digest), &self.ephemeral);
        let (id, compact) = sig.serialize_compact();
        let mut sig_bytes = compact.to_vec();
        sig_bytes.push(i32::from(id) as u8);
        let initiator = node_id(&PublicKey::from_secret_key(SECP256K1, &self.static_key));
        l(vec![b(sig_bytes), b(initiator.to_vec()), b(self.nonce.to_vec()), int(version)])
    }

    /// Completes the handshake from the recipient's ack packet, returning both frame halves.
    pub(crate) fn finish(&self, auth: &[u8], ack: &[u8]) -> Option<(FrameWriter, FrameReader)> {
        let plain = open_packet(&self.static_key, ack)?;
        let (node, _) = super::rlp::parse(&plain)?;
        let Item::List(items) = node.item else { return None };
        let (Item::Bytes(eph), Item::Bytes(nonce)) = (&items.first()?.item, &items.get(1)?.item)
        else {
            return None;
        };
        let remote_ephemeral = public_of_id(eph)?;
        let recipient_nonce = B256::try_from(nonce.as_slice()).ok()?;
        let shared = ecdh_x(&remote_ephemeral, &self.ephemeral);
        let nonce_hash = keccak256([recipient_nonce.as_slice(), self.nonce.as_slice()].concat());
        let shared_secret = keccak256([shared.as_slice(), nonce_hash.as_slice()].concat());
        let aes = keccak256([shared.as_slice(), shared_secret.as_slice()].concat());
        let mac = keccak256([shared.as_slice(), aes.as_slice()].concat());
        let mac_state = |nonce: &B256, packet: &[u8]| {
            let mut state = Keccak256::new();
            state.update((mac ^ *nonce).as_slice());
            state.update(packet);
            state
        };
        let writer = FrameWriter {
            aes: ctr::Ctr128BE::<aes::Aes256>::new(aes.as_slice().into(), &[0u8; 16].into()),
            mac_cipher: aes::Aes256::new(mac.as_slice().into()),
            mac: mac_state(&recipient_nonce, auth),
        };
        let reader = FrameReader {
            aes: ctr::Ctr128BE::<aes::Aes256>::new(aes.as_slice().into(), &[0u8; 16].into()),
            mac_cipher: aes::Aes256::new(mac.as_slice().into()),
            mac: mac_state(&self.nonce, ack),
        };
        Some((writer, reader))
    }
}

fn digest16(mac: &Keccak256) -> [u8; 16] {
    mac.clone().finalize()[..16].try_into().unwrap()
}

fn header_mac(cipher: &aes::Aes256, mac: &mut Keccak256, header_cipher: &[u8]) -> [u8; 16] {
    let mut seed = digest16(mac);
    cipher.encrypt_block((&mut seed).into());
    for (s, h) in seed.iter_mut().zip(header_cipher) {
        *s ^= h;
    }
    mac.update(seed);
    digest16(mac)
}

fn body_mac(cipher: &aes::Aes256, mac: &mut Keccak256, body_cipher: &[u8]) -> [u8; 16] {
    mac.update(body_cipher);
    let previous = digest16(mac);
    let mut seed = previous;
    cipher.encrypt_block((&mut seed).into());
    for (s, p) in seed.iter_mut().zip(previous) {
        *s ^= p;
    }
    mac.update(seed);
    digest16(mac)
}

/// The sending half of a frame codec, able to write any header.
pub(crate) struct FrameWriter {
    aes: ctr::Ctr128BE<aes::Aes256>,
    mac_cipher: aes::Aes256,
    mac: Keccak256,
}

impl std::fmt::Debug for FrameWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameWriter")
    }
}

/// The standard header plaintext for a frame of `size` bytes: size, then RLP `[0, 0]`.
pub(crate) fn header_plain(size: usize) -> [u8; 16] {
    let mut header = [0u8; 16];
    header[..3].copy_from_slice(&(size as u32).to_be_bytes()[1..]);
    header[3..6].copy_from_slice(&[0xc2, 0x80, 0x80]);
    header
}

impl FrameWriter {
    /// Encrypts and MACs a 16-byte header plaintext.
    pub(crate) fn header(&mut self, plain: [u8; 16]) -> Vec<u8> {
        let mut header = plain;
        self.aes.apply_keystream(&mut header);
        let tag = header_mac(&self.mac_cipher, &mut self.mac, &header);
        let mut out = header.to_vec();
        out.extend_from_slice(&tag);
        out
    }

    /// Encrypts and MACs a body, zero-padded to a multiple of 16.
    pub(crate) fn body(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut body = plain.to_vec();
        body.resize(plain.len().next_multiple_of(16), 0);
        self.aes.apply_keystream(&mut body);
        let tag = body_mac(&self.mac_cipher, &mut self.mac, &body);
        body.extend_from_slice(&tag);
        body
    }

    /// One standard frame carrying `payload`.
    pub(crate) fn frame(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut out = self.header(header_plain(payload.len()));
        out.extend(self.body(payload));
        out
    }
}

/// The receiving half of a frame codec.
pub(crate) struct FrameReader {
    aes: ctr::Ctr128BE<aes::Aes256>,
    mac_cipher: aes::Aes256,
    mac: Keccak256,
}

impl std::fmt::Debug for FrameReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameReader")
    }
}

impl FrameReader {
    /// Checks and decrypts a 32-byte header, returning the body size.
    pub(crate) fn header(&mut self, data: &[u8; 32]) -> Option<usize> {
        let mut header: [u8; 16] = data[..16].try_into().unwrap();
        if header_mac(&self.mac_cipher, &mut self.mac, &header) != data[16..] {
            return None;
        }
        self.aes.apply_keystream(&mut header);
        Some(u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize)
    }

    /// Checks and decrypts a body of `size` payload bytes (`data` holds padding and MAC).
    pub(crate) fn body(&mut self, data: &[u8], size: usize) -> Option<Vec<u8>> {
        let (cipher, tag) = data.split_at(data.len().checked_sub(16)?);
        if body_mac(&self.mac_cipher, &mut self.mac, cipher) != tag {
            return None;
        }
        let mut plain = cipher.to_vec();
        self.aes.apply_keystream(&mut plain);
        plain.truncate(size);
        Some(plain)
    }
}
