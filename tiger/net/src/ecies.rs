//! RLPx transport encryption: the ECIES handshake (EIP-8 format) and the frame codec.
//!
//! Spec: <https://github.com/ethereum/devp2p/blob/master/rlpx.md>. The initiator sends `auth`,
//! the recipient answers `ack`, and both derive the AES-256-CTR frame keys and the two keccak
//! MAC states from the ephemeral ECDH secret and both nonces. Only the EIP-8 packet format is
//! produced and accepted; every client in use sends it.

use alloy_primitives::{B256, Keccak256};
use alloy_rlp::{Encodable, Header};
use secp256k1::{PublicKey, SecretKey};

use crate::crypto::{
    Aes256Block, Aes256Ctr, PeerId, aes128_ctr, concat_kdf_32, ecdh_x, hmac_sha256, keccak,
    peer_id, peer_id_of_secret, public_key, random_bytes, random_secret_key, recover, sha256,
    sign_recoverable,
};

/// Largest handshake packet accepted (EIP-8 packets are about 300 to 600 bytes).
pub(crate) const MAX_HANDSHAKE_PACKET: usize = 2048;
/// Largest RLPx frame payload accepted; frames carry whole messages, capped at 16 MiB.
pub(crate) const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
/// Bytes of ECIES overhead: ephemeral key (65), IV (16), and HMAC tag (32).
const ECIES_OVERHEAD: usize = 65 + 16 + 32;
/// The handshake version written into auth and ack bodies.
const HANDSHAKE_VERSION: u8 = 4;

/// Errors of the RLPx handshake and frame layers. Any of them ends the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EciesError {
    /// The packet is shorter than its framing requires or longer than the limit.
    BadLength,
    /// The ECIES or frame MAC does not authenticate.
    BadMac,
    /// A public key or signature is not valid secp256k1.
    BadKey,
    /// The handshake body is not the expected RLP.
    BadRlp,
}

impl std::fmt::Display for EciesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Encrypts `plain` to `remote` with RLPx's ECIES; `auth_data` is MACed but not sent.
fn ecies_encrypt(remote: &PublicKey, plain: &[u8], auth_data: &[u8]) -> Vec<u8> {
    let ephemeral = random_secret_key();
    let mut iv = [0u8; 16];
    random_bytes(&mut iv);
    let key = concat_kdf_32(&ecdh_x(remote, &ephemeral));
    let enc_key: [u8; 16] = key[..16].try_into().unwrap();
    let mac_key = sha256(&[&key[16..]]);
    let mut out = Vec::with_capacity(plain.len() + ECIES_OVERHEAD);
    out.extend_from_slice(&PublicKey::from_secret_key_global(&ephemeral).serialize_uncompressed());
    out.extend_from_slice(&iv);
    let start = out.len();
    out.extend_from_slice(plain);
    aes128_ctr(&enc_key, &iv, &mut out[start..]);
    let tag = hmac_sha256(&mac_key, &[&iv, &out[start..], auth_data]);
    out.extend_from_slice(&tag);
    out
}

/// Decrypts an ECIES message addressed to `secret`.
fn ecies_decrypt(secret: &SecretKey, data: &[u8], auth_data: &[u8]) -> Result<Vec<u8>, EciesError> {
    if data.len() < ECIES_OVERHEAD {
        return Err(EciesError::BadLength);
    }
    let remote = PublicKey::from_slice(&data[..65]).map_err(|_| EciesError::BadKey)?;
    let iv: [u8; 16] = data[65..81].try_into().unwrap();
    let (cipher_text, tag) = data[81..].split_at(data.len() - 81 - 32);
    let key = concat_kdf_32(&ecdh_x(&remote, secret));
    let enc_key: [u8; 16] = key[..16].try_into().unwrap();
    let mac_key = sha256(&[&key[16..]]);
    if hmac_sha256(&mac_key, &[&iv, cipher_text, auth_data]) != tag {
        return Err(EciesError::BadMac);
    }
    let mut plain = cipher_text.to_vec();
    aes128_ctr(&enc_key, &iv, &mut plain);
    Ok(plain)
}

/// Wraps an RLP body in an EIP-8 packet: 2-byte size prefix, then the ECIES ciphertext of the
/// body followed by 100 to 300 bytes of random padding.
fn seal_packet(remote: &PublicKey, body: &[u8]) -> Vec<u8> {
    let mut pad = [0u8; 1];
    random_bytes(&mut pad);
    let mut plain = body.to_vec();
    plain.resize(body.len() + 100 + (pad[0] as usize % 201), 0);
    random_bytes(&mut plain[body.len()..]);
    let size = (plain.len() + ECIES_OVERHEAD) as u16;
    let mut packet = size.to_be_bytes().to_vec();
    packet.extend(ecies_encrypt(remote, &plain, &size.to_be_bytes()));
    packet
}

/// Opens an EIP-8 packet (size prefix included) and returns the RLP list payload's items.
fn open_packet(secret: &SecretKey, packet: &[u8]) -> Result<Vec<u8>, EciesError> {
    if packet.len() < 2 || packet.len() > MAX_HANDSHAKE_PACKET {
        return Err(EciesError::BadLength);
    }
    let size = u16::from_be_bytes([packet[0], packet[1]]) as usize;
    if size != packet.len() - 2 {
        return Err(EciesError::BadLength);
    }
    ecies_decrypt(secret, &packet[2..], &packet[..2])
}

/// Returns the total length of an EIP-8 handshake packet from its first two bytes.
pub(crate) fn handshake_packet_len(prefix: [u8; 2]) -> Result<usize, EciesError> {
    let len = u16::from_be_bytes(prefix) as usize + 2;
    if len > MAX_HANDSHAKE_PACKET || len < 2 + ECIES_OVERHEAD {
        return Err(EciesError::BadLength);
    }
    Ok(len)
}

fn decode_bytes<'a>(buf: &mut &'a [u8]) -> Result<&'a [u8], EciesError> {
    let header = Header::decode(buf).map_err(|_| EciesError::BadRlp)?;
    if header.list || buf.len() < header.payload_length {
        return Err(EciesError::BadRlp);
    }
    let (value, rest) = buf.split_at(header.payload_length);
    *buf = rest;
    Ok(value)
}

fn decode_fixed<const N: usize>(buf: &mut &[u8]) -> Result<[u8; N], EciesError> {
    decode_bytes(buf)?.try_into().map_err(|_| EciesError::BadRlp)
}

/// Returns the payload of the leading RLP list, ignoring trailing data (EIP-8).
fn list_payload(body: &[u8]) -> Result<&[u8], EciesError> {
    let mut buf = body;
    let header = Header::decode(&mut buf).map_err(|_| EciesError::BadRlp)?;
    if !header.list || buf.len() < header.payload_length {
        return Err(EciesError::BadRlp);
    }
    Ok(&buf[..header.payload_length])
}

fn encode_list(items: &[&dyn Encodable]) -> Vec<u8> {
    let payload_length = items.iter().map(|item| item.length()).sum();
    let mut out = Vec::new();
    Header { list: true, payload_length }.encode(&mut out);
    for item in items {
        item.encode(&mut out);
    }
    out
}

fn xor(a: &B256, b: &B256) -> B256 {
    let mut out = *a;
    for (o, x) in out.0.iter_mut().zip(b.0) {
        *o ^= x;
    }
    out
}

/// The secrets both sides derive at the end of the handshake.
struct Secrets {
    aes: B256,
    mac: B256,
}

impl Secrets {
    fn derive(ephemeral_shared: &B256, recipient_nonce: &B256, initiator_nonce: &B256) -> Self {
        let nonce_hash = keccak(&[recipient_nonce.as_slice(), initiator_nonce.as_slice()]);
        let shared = keccak(&[ephemeral_shared.as_slice(), nonce_hash.as_slice()]);
        let aes = keccak(&[ephemeral_shared.as_slice(), shared.as_slice()]);
        let mac = keccak(&[ephemeral_shared.as_slice(), aes.as_slice()]);
        Self { aes, mac }
    }

    fn mac_state(&self, nonce: &B256, packet: &[u8]) -> Keccak256 {
        let mut state = Keccak256::new();
        state.update(xor(&self.mac, nonce));
        state.update(packet);
        state
    }
}

/// The initiator side of a handshake in progress.
#[derive(Debug)]
pub struct Initiator {
    ephemeral: SecretKey,
    nonce: B256,
    auth: Vec<u8>,
}

impl Initiator {
    /// Starts a handshake with `remote` and returns the state plus the auth packet to send.
    pub fn new(static_key: &SecretKey, remote: &PeerId) -> Result<Self, EciesError> {
        let mut nonce = B256::ZERO;
        random_bytes(&mut nonce.0);
        Self::with_keys(static_key, remote, random_secret_key(), nonce)
    }

    fn with_keys(
        static_key: &SecretKey,
        remote_id: &PeerId,
        ephemeral: SecretKey,
        nonce: B256,
    ) -> Result<Self, EciesError> {
        let remote = public_key(remote_id).ok_or(EciesError::BadKey)?;
        let static_shared = ecdh_x(&remote, static_key);
        let sig = sign_recoverable(xor(&static_shared, &nonce), &ephemeral);
        let id = peer_id_of_secret(static_key);
        let body = encode_list(&[&&sig[..], &id, &nonce, &HANDSHAKE_VERSION]);
        let auth = seal_packet(&remote, &body);
        Ok(Self { ephemeral, nonce, auth })
    }

    /// The auth packet to write to the socket.
    pub fn auth_packet(&self) -> &[u8] {
        &self.auth
    }

    /// Completes the handshake with the recipient's ack packet (size prefix included).
    pub fn finish(self, static_key: &SecretKey, ack: &[u8]) -> Result<FrameCodec, EciesError> {
        let body = open_packet(static_key, ack)?;
        let mut items = list_payload(&body)?;
        let remote_ephemeral: [u8; 64] = decode_fixed(&mut items)?;
        let recipient_nonce = B256::from(decode_fixed::<32>(&mut items)?);
        let remote_ephemeral =
            public_key(&PeerId::from(remote_ephemeral)).ok_or(EciesError::BadKey)?;
        let shared = ecdh_x(&remote_ephemeral, &self.ephemeral);
        let secrets = Secrets::derive(&shared, &recipient_nonce, &self.nonce);
        let egress = secrets.mac_state(&recipient_nonce, &self.auth);
        let ingress = secrets.mac_state(&self.nonce, ack);
        Ok(FrameCodec::new(&secrets, egress, ingress))
    }
}

/// The result of accepting an auth packet: who connected, the ack to send, and the codec.
#[derive(Debug)]
pub struct Accepted {
    /// The initiator's node id.
    pub remote: PeerId,
    /// The ack packet to write to the socket.
    pub ack: Vec<u8>,
    /// The frame codec for the rest of the connection.
    pub codec: FrameCodec,
}

/// Accepts an auth packet (size prefix included) as the recipient.
pub(crate) fn accept(static_key: &SecretKey, auth: &[u8]) -> Result<Accepted, EciesError> {
    let mut nonce = B256::ZERO;
    random_bytes(&mut nonce.0);
    accept_with_keys(static_key, auth, random_secret_key(), nonce)
}

fn accept_with_keys(
    static_key: &SecretKey,
    auth: &[u8],
    ephemeral: SecretKey,
    nonce: B256,
) -> Result<Accepted, EciesError> {
    let body = open_packet(static_key, auth)?;
    let mut items = list_payload(&body)?;
    let sig: [u8; 65] = decode_fixed(&mut items)?;
    let remote_id = PeerId::from(decode_fixed::<64>(&mut items)?);
    let initiator_nonce = B256::from(decode_fixed::<32>(&mut items)?);
    let remote = public_key(&remote_id).ok_or(EciesError::BadKey)?;
    let static_shared = ecdh_x(&remote, static_key);
    let remote_ephemeral =
        recover(xor(&static_shared, &initiator_nonce), &sig).ok_or(EciesError::BadKey)?;
    let ephemeral_public = peer_id(&PublicKey::from_secret_key_global(&ephemeral));
    let body = encode_list(&[&ephemeral_public, &nonce, &HANDSHAKE_VERSION]);
    let ack = seal_packet(&remote, &body);
    let shared = ecdh_x(&remote_ephemeral, &ephemeral);
    let secrets = Secrets::derive(&shared, &nonce, &initiator_nonce);
    let egress = secrets.mac_state(&initiator_nonce, &ack);
    let ingress = secrets.mac_state(&nonce, auth);
    Ok(Accepted { remote: remote_id, ack, codec: FrameCodec::new(&secrets, egress, ingress) })
}

/// Encrypts and authenticates RLPx frames in both directions after the handshake.
#[derive(Debug)]
pub struct FrameCodec {
    /// The sending half.
    pub egress: FrameEgress,
    /// The receiving half.
    pub ingress: FrameIngress,
}

/// The sending half of a [`FrameCodec`].
pub struct FrameEgress {
    aes: Aes256Ctr,
    mac_cipher: Aes256Block,
    mac: Keccak256,
}

/// The receiving half of a [`FrameCodec`].
pub struct FrameIngress {
    aes: Aes256Ctr,
    mac_cipher: Aes256Block,
    mac: Keccak256,
}

impl std::fmt::Debug for FrameEgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameEgress")
    }
}

impl std::fmt::Debug for FrameIngress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameIngress")
    }
}

/// The fixed header-data of every frame: RLP `[0, 0]` (capability-id and context-id, unused).
const HEADER_DATA: [u8; 3] = [0xc2, 0x80, 0x80];

fn digest16(mac: &Keccak256) -> [u8; 16] {
    mac.clone().finalize()[..16].try_into().unwrap()
}

fn header_mac(cipher: &Aes256Block, mac: &mut Keccak256, header_cipher: &[u8]) -> [u8; 16] {
    let mut seed = digest16(mac);
    cipher.encrypt(&mut seed);
    for (s, h) in seed.iter_mut().zip(header_cipher) {
        *s ^= h;
    }
    mac.update(seed);
    digest16(mac)
}

fn body_mac(cipher: &Aes256Block, mac: &mut Keccak256, body_cipher: &[u8]) -> [u8; 16] {
    mac.update(body_cipher);
    let previous = digest16(mac);
    let mut seed = previous;
    cipher.encrypt(&mut seed);
    for (s, p) in seed.iter_mut().zip(previous) {
        *s ^= p;
    }
    mac.update(seed);
    digest16(mac)
}

impl FrameCodec {
    fn new(secrets: &Secrets, egress_mac: Keccak256, ingress_mac: Keccak256) -> Self {
        Self {
            egress: FrameEgress {
                aes: Aes256Ctr::new(&secrets.aes),
                mac_cipher: Aes256Block::new(&secrets.mac),
                mac: egress_mac,
            },
            ingress: FrameIngress {
                aes: Aes256Ctr::new(&secrets.aes),
                mac_cipher: Aes256Block::new(&secrets.mac),
                mac: ingress_mac,
            },
        }
    }

    /// Bytes on the wire for a frame body of `size` payload bytes: padding plus the 16-byte MAC.
    pub const fn body_len(size: usize) -> usize {
        size.next_multiple_of(16) + 16
    }
}

impl FrameEgress {
    /// Appends one encrypted frame carrying `payload` to `out`.
    pub fn encode(&mut self, payload: &[u8], out: &mut Vec<u8>) {
        assert!(!payload.is_empty() && payload.len() < MAX_FRAME_SIZE);
        let mut header = [0u8; 16];
        header[..3].copy_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        header[3..6].copy_from_slice(&HEADER_DATA);
        self.aes.apply(&mut header);
        let header_mac = header_mac(&self.mac_cipher, &mut self.mac, &header);
        out.extend_from_slice(&header);
        out.extend_from_slice(&header_mac);
        let start = out.len();
        out.extend_from_slice(payload);
        out.resize(start + payload.len().next_multiple_of(16), 0);
        self.aes.apply(&mut out[start..]);
        let body_mac = body_mac(&self.mac_cipher, &mut self.mac, &out[start..]);
        out.extend_from_slice(&body_mac);
    }
}

impl FrameIngress {
    /// Checks and decrypts a 32-byte frame header, returning the payload size. The caller then
    /// reads [`FrameCodec::body_len`] bytes and passes them to [`Self::decode_body`].
    pub fn decode_header(&mut self, header: &mut [u8; 32]) -> Result<usize, EciesError> {
        let (cipher_text, mac) = header.split_at_mut(16);
        let expected = header_mac(&self.mac_cipher, &mut self.mac, cipher_text);
        if expected != *mac {
            return Err(EciesError::BadMac);
        }
        self.aes.apply(cipher_text);
        let size = u32::from_be_bytes([0, cipher_text[0], cipher_text[1], cipher_text[2]]) as usize;
        if size == 0 || size > MAX_FRAME_SIZE {
            return Err(EciesError::BadLength);
        }
        Ok(size)
    }

    /// Checks and decrypts a frame body in place and returns the payload.
    pub fn decode_body<'a>(
        &mut self,
        body: &'a mut [u8],
        size: usize,
    ) -> Result<&'a [u8], EciesError> {
        if body.len() != FrameCodec::body_len(size) {
            return Err(EciesError::BadLength);
        }
        let (cipher_text, mac) = body.split_at_mut(body.len() - 16);
        let expected = body_mac(&self.mac_cipher, &mut self.mac, cipher_text);
        if expected != *mac {
            return Err(EciesError::BadMac);
        }
        self.aes.apply(cipher_text);
        Ok(&cipher_text[..size])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    // Test vectors from EIP-8: node A initiates a connection with node B.
    const STATIC_A: &str = "49a7b37aa6f6645917e7b807e9d1c00d4fa71f18343b0d4122a4d2df64dd6fee";
    const STATIC_B: &str = "b71c71a67e1177ad4e901695e1b4b9ee17ae16c6668d313eac2f96dbcda3f291";
    const EPHEMERAL_A: &str = "869d6ecf5211f1cc60418a13b9d870b22959d0c16f02bec714c960dd2298a32d";
    const EPHEMERAL_B: &str = "e238eb8e04fee6511ab04c6dd3c89ce097b11f25d584863ac2b6d5b35b1847e4";
    const NONCE_A: &str = "7e968bba13b6c50e2c4cd7f241cc0d64d1ac25c7f5952df231ac6a2bda8ee5d6";
    const NONCE_B: &str = "559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd";

    const AUTH_2: &str = "01b304ab7578555167be8154d5cc456f567d5ba302662433674222360f08d5f1534499d3678b513b0fca474f3a514b18e75683032eb63fccb16c156dc6eb2c0b1593f0d84ac74f6e475f1b8d56116b849634a8c458705bf83a626ea0384d4d7341aae591fae42ce6bd5c850bfe0b999a694a49bbbaf3ef6cda61110601d3b4c02ab6c30437257a6e0117792631a4b47c1d52fc0f8f89caadeb7d02770bf999cc147d2df3b62e1ffb2c9d8c125a3984865356266bca11ce7d3a688663a51d82defaa8aad69da39ab6d5470e81ec5f2a7a47fb865ff7cca21516f9299a07b1bc63ba56c7a1a892112841ca44b6e0034dee70c9adabc15d76a54f443593fafdc3b27af8059703f88928e199cb122362a4b35f62386da7caad09c001edaeb5f8a06d2b26fb6cb93c52a9fca51853b68193916982358fe1e5369e249875bb8d0d0ec36f917bc5e1eafd5896d46bd61ff23f1a863a8a8dcd54c7b109b771c8e61ec9c8908c733c0263440e2aa067241aaa433f0bb053c7b31a838504b148f570c0ad62837129e547678c5190341e4f1693956c3bf7678318e2d5b5340c9e488eefea198576344afbdf66db5f51204a6961a63ce072c8926c";
    const AUTH_3: &str = "01b8044c6c312173685d1edd268aa95e1d495474c6959bcdd10067ba4c9013df9e40ff45f5bfd6f72471f93a91b493f8e00abc4b80f682973de715d77ba3a005a242eb859f9a211d93a347fa64b597bf280a6b88e26299cf263b01b8dfdb712278464fd1c25840b995e84d367d743f66c0e54a586725b7bbf12acca27170ae3283c1073adda4b6d79f27656993aefccf16e0d0409fe07db2dc398a1b7e8ee93bcd181485fd332f381d6a050fba4c7641a5112ac1b0b61168d20f01b479e19adf7fdbfa0905f63352bfc7e23cf3357657455119d879c78d3cf8c8c06375f3f7d4861aa02a122467e069acaf513025ff196641f6d2810ce493f51bee9c966b15c5043505350392b57645385a18c78f14669cc4d960446c17571b7c5d725021babbcd786957f3d17089c084907bda22c2b2675b4378b114c601d858802a55345a15116bc61da4193996187ed70d16730e9ae6b3bb8787ebcaea1871d850997ddc08b4f4ea668fbf37407ac044b55be0908ecb94d4ed172ece66fd31bfdadf2b97a8bc690163ee11f5b575a4b44e36e2bfb2f0fce91676fd64c7773bac6a003f481fddd0bae0a1f31aa27504e2a533af4cef3b623f4791b2cca6d490";
    const ACK_2: &str = "01ea0451958701280a56482929d3b0757da8f7fbe5286784beead59d95089c217c9b917788989470b0e330cc6e4fb383c0340ed85fab836ec9fb8a49672712aeabbdfd1e837c1ff4cace34311cd7f4de05d59279e3524ab26ef753a0095637ac88f2b499b9914b5f64e143eae548a1066e14cd2f4bd7f814c4652f11b254f8a2d0191e2f5546fae6055694aed14d906df79ad3b407d94692694e259191cde171ad542fc588fa2b7333313d82a9f887332f1dfc36cea03f831cb9a23fea05b33deb999e85489e645f6aab1872475d488d7bd6c7c120caf28dbfc5d6833888155ed69d34dbdc39c1f299be1057810f34fbe754d021bfca14dc989753d61c413d261934e1a9c67ee060a25eefb54e81a4d14baff922180c395d3f998d70f46f6b58306f969627ae364497e73fc27f6d17ae45a413d322cb8814276be6ddd13b885b201b943213656cde498fa0e9ddc8e0b8f8a53824fbd82254f3e2c17e8eaea009c38b4aa0a3f306e8797db43c25d68e86f262e564086f59a2fc60511c42abfb3057c247a8a8fe4fb3ccbadde17514b7ac8000cdb6a912778426260c47f38919a91f25f4b5ffb455d6aaaf150f7e5529c100ce62d6d92826a71778d809bdf60232ae21ce8a437eca8223f45ac37f6487452ce626f549b3b5fdee26afd2072e4bc75833c2464c805246155289f4";
    const ACK_3: &str = "01f004076e58aae772bb101ab1a8e64e01ee96e64857ce82b1113817c6cdd52c09d26f7b90981cd7ae835aeac72e1573b8a0225dd56d157a010846d888dac7464baf53f2ad4e3d584531fa203658fab03a06c9fd5e35737e417bc28c1cbf5e5dfc666de7090f69c3b29754725f84f75382891c561040ea1ddc0d8f381ed1b9d0d4ad2a0ec021421d847820d6fa0ba66eaf58175f1b235e851c7e2124069fbc202888ddb3ac4d56bcbd1b9b7eab59e78f2e2d400905050f4a92dec1c4bdf797b3fc9b2f8e84a482f3d800386186712dae00d5c386ec9387a5e9c9a1aca5a573ca91082c7d68421f388e79127a5177d4f8590237364fd348c9611fa39f78dcdceee3f390f07991b7b47e1daa3ebcb6ccc9607811cb17ce51f1c8c2c5098dbdd28fca547b3f58c01a424ac05f869f49c6a34672ea2cbbc558428aa1fe48bbfd61158b1b735a65d99f21e70dbc020bfdface9f724a0d1fb5895db971cc81aa7608baa0920abb0a565c9c436e2fd13323428296c86385f2384e408a31e104670df0791d93e743a3a5194ee6b076fb6323ca593011b7348c16cf58f66b9633906ba54a2ee803187344b394f75dd2e663a57b956cb830dd7a908d4f39a2336a61ef9fda549180d4ccde21514d117b6c6fd07a9102b5efe710a32af4eeacae2cb3b1dec035b9593b48b9d3ca4c13d245d5f04169b0b1";

    fn key(hex_str: &str) -> SecretKey {
        SecretKey::from_byte_array(hex::decode(hex_str).unwrap().try_into().unwrap()).unwrap()
    }

    fn nonce(hex_str: &str) -> B256 {
        B256::from_slice(&hex::decode(hex_str).unwrap())
    }

    #[test]
    fn recipient_decodes_eip8_auth_packets() {
        for auth in [AUTH_2, AUTH_3] {
            let auth = hex::decode(auth).unwrap();
            let accepted =
                accept_with_keys(&key(STATIC_B), &auth, key(EPHEMERAL_B), nonce(NONCE_B)).unwrap();
            assert_eq!(accepted.remote, peer_id_of_secret(&key(STATIC_A)));
        }
    }

    #[test]
    fn eip8_vectors_derive_the_published_secrets() {
        // Node B's view of (Auth2, Ack2): B receives Auth2 and sent Ack2, so its ingress MAC is
        // seeded with Auth2 and its secrets come from both ephemeral keys and nonces.
        let auth = hex::decode(AUTH_2).unwrap();
        let shared =
            ecdh_x(&PublicKey::from_secret_key_global(&key(EPHEMERAL_A)), &key(EPHEMERAL_B));
        let secrets = Secrets::derive(&shared, &nonce(NONCE_B), &nonce(NONCE_A));
        assert_eq!(
            hex::encode(secrets.aes),
            "80e8632c05fed6fc2a13b0f8d31a3cf645366239170ea067065aba8e28bac487"
        );
        assert_eq!(
            hex::encode(secrets.mac),
            "2ea74ec5dae199227dff1af715362700e989d889d7a493cb0639691efb8e5f98"
        );
        let mut ingress = secrets.mac_state(&nonce(NONCE_B), &auth);
        ingress.update(b"foo");
        assert_eq!(
            hex::encode(ingress.finalize()),
            "0c7ec6340062cc46f5e9f1e3cf86f8c8c403c5a0964f5df0ebd34a75ddc86db5"
        );

        // Node A's side decodes both published ack packets.
        for ack_hex in [ACK_2, ACK_3] {
            let initiator = Initiator {
                ephemeral: key(EPHEMERAL_A),
                nonce: nonce(NONCE_A),
                auth: auth.clone(),
            };
            initiator.finish(&key(STATIC_A), &hex::decode(ack_hex).unwrap()).unwrap();
        }
    }

    #[test]
    fn handshake_and_frames_round_trip() {
        let a = random_secret_key();
        let b = random_secret_key();
        let initiator = Initiator::new(&a, &peer_id_of_secret(&b)).unwrap();
        let auth = initiator.auth_packet().to_vec();
        assert_eq!(handshake_packet_len([auth[0], auth[1]]).unwrap(), auth.len());
        let accepted = accept(&b, &auth).unwrap();
        assert_eq!(accepted.remote, peer_id_of_secret(&a));
        let mut a_codec = initiator.finish(&a, &accepted.ack).unwrap();
        let mut b_codec = accepted.codec;

        {
            let (sender, receiver) = (&mut a_codec.egress, &mut b_codec.ingress);
            for payload in [&b"x"[..], &[7u8; 16][..], &[9u8; 1000][..]] {
                let mut wire = Vec::new();
                sender.encode(payload, &mut wire);
                let mut header: [u8; 32] = wire[..32].try_into().unwrap();
                let size = receiver.decode_header(&mut header).unwrap();
                assert_eq!(size, payload.len());
                let body = &mut wire[32..];
                assert_eq!(receiver.decode_body(body, size).unwrap(), payload);
            }
        }
        // A tampered frame fails the MAC.
        let mut wire = Vec::new();
        b_codec.egress.encode(b"hello", &mut wire);
        wire[40] ^= 1;
        let mut header: [u8; 32] = wire[..32].try_into().unwrap();
        let size = a_codec.ingress.decode_header(&mut header).unwrap();
        assert_eq!(a_codec.ingress.decode_body(&mut wire[32..], size), Err(EciesError::BadMac));
    }
}
