//! The tp2p handshake and transport encryption: `Noise_XK_secp256k1_ChaChaPoly_SHA256` exactly as
//! Lightning's BOLT 8 specifies it (acts of 50, 50 and 66 bytes; length-prefixed AEAD messages of
//! at most 65,535 bytes; each key rotated after 1,000 uses), with tp2p's own prologue in place of
//! `lightning`. The prologue binds the network (chain id, genesis hash, layer), so peers of
//! another chain or layer fail at Act One's tag. The BOLT 8 test vectors pass with the
//! `lightning` prologue (tests below).
//!
//! Sans-IO: callers feed the fixed-size acts and frames they read and send what these functions
//! return. Ephemeral keys are passed in so tests and simulations are deterministic.

use ring::{
    aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey},
    digest, hkdf,
};
use secp256k1::{PublicKey, SECP256K1, SecretKey, ecdh::SharedSecret};

/// `Noise_XK_secp256k1_ChaChaPoly_SHA256`, BOLT 8's protocol name.
pub const PROTOCOL_NAME: &[u8] = b"Noise_XK_secp256k1_ChaChaPoly_SHA256";
/// The handshake version byte that leads each act.
pub const HANDSHAKE_VERSION: u8 = 0;
/// Act One and Act Two: version, a compressed key, a tag.
pub const ACT_ONE_LEN: usize = 50;
/// Act Two has Act One's layout.
pub const ACT_TWO_LEN: usize = 50;
/// Act Three: version, the encrypted static key with its tag, a tag.
pub const ACT_THREE_LEN: usize = 66;
/// The Poly1305 tag.
pub const TAG_LEN: usize = 16;
/// The encrypted length prefix of a message: two bytes and a tag.
pub const LENGTH_HEADER_LEN: usize = 2 + TAG_LEN;
/// The largest message plaintext.
pub const MAX_MESSAGE_LEN: usize = 65_535;
/// A key is rotated once its nonce reaches this value.
const KEY_ROTATION_NONCE: u64 = 1000;

/// Why a handshake act or a message was refused. Every variant ends the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseError {
    /// The act's leading version byte is not [`HANDSHAKE_VERSION`].
    BadVersion(u8),
    /// The act's key is not a valid compressed secp256k1 point.
    BadPublicKey,
    /// A tag did not verify: a different prologue, an unknown responder key, or tampering.
    BadTag,
}

impl std::fmt::Display for NoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadVersion(v) => write!(f, "handshake version {v}"),
            Self::BadPublicKey => f.write_str("bad public key"),
            Self::BadTag => f.write_str("bad tag"),
        }
    }
}

impl std::error::Error for NoiseError {}

/// BOLT 8's `ECDH(k, rk)`: SHA-256 of the compressed shared point (libsecp256k1's default hash).
fn ecdh(public: &PublicKey, secret: &SecretKey) -> [u8; 32] {
    SharedSecret::new(public, secret).secret_bytes()
}

pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut context = digest::Context::new(&digest::SHA256);
    for part in parts {
        context.update(part);
    }
    context.finish().as_ref().try_into().expect("SHA-256 output is 32 bytes")
}

struct Okm64;

impl hkdf::KeyType for Okm64 {
    fn len(&self) -> usize {
        64
    }
}

/// BOLT 8's `HKDF(salt, ikm)`: RFC 5869 with an empty `info`, 64 bytes split in two.
fn hkdf2(salt: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    let okm = prk.expand(&[], Okm64).expect("64 bytes is within HKDF-SHA256's output limit");
    let mut out = [0u8; 64];
    okm.fill(&mut out).expect("64 bytes is within HKDF-SHA256's output limit");
    (out[..32].try_into().expect("32 bytes"), out[32..].try_into().expect("32 bytes"))
}

/// 32 zero bits, then the 64-bit counter little-endian (the Noise convention).
fn nonce(n: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&n.to_le_bytes());
    Nonce::assume_unique_for_key(bytes)
}

fn aead_key(k: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, k).expect("ChaCha20-Poly1305 key is 32 bytes"),
    )
}

/// `encryptWithAD`: appends the ciphertext and tag of `plaintext` to `out`.
fn encrypt_with_ad(k: &[u8; 32], n: u64, ad: &[u8], plaintext: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(plaintext);
    let mut body = out.split_off(start);
    aead_key(k)
        .seal_in_place_append_tag(nonce(n), Aad::from(ad), &mut body)
        .expect("plaintext is far below ChaCha20-Poly1305's limit");
    out.extend_from_slice(&body);
}

/// `decryptWithAD`: decrypts `ciphertext` (with its trailing tag) in place, returning the
/// plaintext's length.
fn decrypt_with_ad(
    k: &[u8; 32],
    n: u64,
    ad: &[u8],
    ciphertext: &mut [u8],
) -> Result<usize, NoiseError> {
    aead_key(k)
        .open_in_place(nonce(n), Aad::from(ad), ciphertext)
        .map(|plain| plain.len())
        .map_err(|_| NoiseError::BadTag)
}

fn parse_key(bytes: &[u8]) -> Result<PublicKey, NoiseError> {
    PublicKey::from_slice(bytes).map_err(|_| NoiseError::BadPublicKey)
}

/// The handshake digest `h` and chaining key `ck`.
#[derive(Clone)]
struct SymmetricState {
    h: [u8; 32],
    ck: [u8; 32],
}

impl SymmetricState {
    /// Initialization: `h = SHA-256(name)`, `ck = h`, `h = SHA-256(h || prologue)`, then the
    /// responder's static key.
    fn new(prologue: &[u8], responder_static: &PublicKey) -> Self {
        let h = sha256(&[PROTOCOL_NAME]);
        let mut state = Self { h, ck: h };
        state.mix_hash(prologue);
        state.mix_hash(&responder_static.serialize());
        state
    }

    fn mix_hash(&mut self, data: &[u8]) {
        self.h = sha256(&[&self.h, data]);
    }

    /// `ck, temp_k = HKDF(ck, ikm)`, returning `temp_k`.
    fn mix_key(&mut self, ikm: &[u8]) -> [u8; 32] {
        let (ck, temp_k) = hkdf2(&self.ck, ikm);
        self.ck = ck;
        temp_k
    }

    /// The session keys after Act Three: `(initiator's send key, responder's send key)`.
    fn split(&self) -> ([u8; 32], [u8; 32]) {
        hkdf2(&self.ck, &[])
    }
}

/// The initiator before Act Two.
pub struct Initiator {
    state: SymmetricState,
    local: SecretKey,
    ephemeral: SecretKey,
    temp_k2: [u8; 32],
}

impl std::fmt::Debug for Initiator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Initiator").finish_non_exhaustive()
    }
}

impl Initiator {
    /// Starts a handshake to the responder `remote`, returning the state and Act One.
    pub fn new(
        local: SecretKey,
        remote: &PublicKey,
        prologue: &[u8],
        ephemeral: SecretKey,
    ) -> (Self, [u8; ACT_ONE_LEN]) {
        let mut state = SymmetricState::new(prologue, remote);
        let e_pub = PublicKey::from_secret_key(SECP256K1, &ephemeral).serialize();
        state.mix_hash(&e_pub);
        let temp_k1 = state.mix_key(&ecdh(remote, &ephemeral));
        let mut c = Vec::with_capacity(TAG_LEN);
        encrypt_with_ad(&temp_k1, 0, &state.h, &[], &mut c);
        state.mix_hash(&c);
        let mut act = [0u8; ACT_ONE_LEN];
        act[0] = HANDSHAKE_VERSION;
        act[1..34].copy_from_slice(&e_pub);
        act[34..].copy_from_slice(&c);
        (Self { state, local, ephemeral, temp_k2: [0; 32] }, act)
    }

    /// Reads Act Two and returns Act Three and the session.
    pub fn act_two(
        mut self,
        act: &[u8; ACT_TWO_LEN],
    ) -> Result<([u8; ACT_THREE_LEN], Session), NoiseError> {
        if act[0] != HANDSHAKE_VERSION {
            return Err(NoiseError::BadVersion(act[0]));
        }
        let re = parse_key(&act[1..34])?;
        self.state.mix_hash(&act[1..34]);
        self.temp_k2 = self.state.mix_key(&ecdh(&re, &self.ephemeral));
        let mut c = act[34..].to_vec();
        decrypt_with_ad(&self.temp_k2, 0, &self.state.h, &mut c)?;
        self.state.mix_hash(&act[34..]);

        // Act Three: -> s, se
        let s_pub = PublicKey::from_secret_key(SECP256K1, &self.local).serialize();
        let mut c = Vec::with_capacity(33 + TAG_LEN);
        encrypt_with_ad(&self.temp_k2, 1, &self.state.h, &s_pub, &mut c);
        self.state.mix_hash(&c);
        let temp_k3 = self.state.mix_key(&ecdh(&re, &self.local));
        let mut t = Vec::with_capacity(TAG_LEN);
        encrypt_with_ad(&temp_k3, 0, &self.state.h, &[], &mut t);
        let (sk, rk) = self.state.split();
        let mut out = [0u8; ACT_THREE_LEN];
        out[0] = HANDSHAKE_VERSION;
        out[1..50].copy_from_slice(&c);
        out[50..].copy_from_slice(&t);
        Ok((out, Session::new(sk, rk, self.state.ck, self.state.h)))
    }
}

/// The responder before Act One.
pub struct Responder {
    state: SymmetricState,
    local: SecretKey,
    ephemeral: SecretKey,
}

impl std::fmt::Debug for Responder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responder").finish_non_exhaustive()
    }
}

impl Responder {
    /// Prepares to answer a handshake with the local static key.
    pub fn new(local: SecretKey, prologue: &[u8], ephemeral: SecretKey) -> Self {
        let local_pub = PublicKey::from_secret_key(SECP256K1, &local);
        Self { state: SymmetricState::new(prologue, &local_pub), local, ephemeral }
    }

    /// Reads Act One and returns the state awaiting Act Three, and Act Two.
    pub fn act_one(
        mut self,
        act: &[u8; ACT_ONE_LEN],
    ) -> Result<(AwaitingActThree, [u8; ACT_TWO_LEN]), NoiseError> {
        if act[0] != HANDSHAKE_VERSION {
            return Err(NoiseError::BadVersion(act[0]));
        }
        let re = parse_key(&act[1..34])?;
        self.state.mix_hash(&act[1..34]);
        let temp_k1 = self.state.mix_key(&ecdh(&re, &self.local));
        let mut c = act[34..].to_vec();
        decrypt_with_ad(&temp_k1, 0, &self.state.h, &mut c)?;
        self.state.mix_hash(&act[34..]);

        // Act Two: <- e, ee
        let e_pub = PublicKey::from_secret_key(SECP256K1, &self.ephemeral).serialize();
        self.state.mix_hash(&e_pub);
        let temp_k2 = self.state.mix_key(&ecdh(&re, &self.ephemeral));
        let mut c = Vec::with_capacity(TAG_LEN);
        encrypt_with_ad(&temp_k2, 0, &self.state.h, &[], &mut c);
        self.state.mix_hash(&c);
        let mut out = [0u8; ACT_TWO_LEN];
        out[0] = HANDSHAKE_VERSION;
        out[1..34].copy_from_slice(&e_pub);
        out[34..].copy_from_slice(&c);
        Ok((AwaitingActThree { state: self.state, ephemeral: self.ephemeral, temp_k2 }, out))
    }
}

/// The responder after Act Two.
pub struct AwaitingActThree {
    state: SymmetricState,
    ephemeral: SecretKey,
    temp_k2: [u8; 32],
}

impl std::fmt::Debug for AwaitingActThree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwaitingActThree").finish_non_exhaustive()
    }
}

impl AwaitingActThree {
    /// Reads Act Three and returns the initiator's static key and the session.
    pub fn act_three(
        mut self,
        act: &[u8; ACT_THREE_LEN],
    ) -> Result<(PublicKey, Session), NoiseError> {
        if act[0] != HANDSHAKE_VERSION {
            return Err(NoiseError::BadVersion(act[0]));
        }
        let mut c = act[1..50].to_vec();
        let len = decrypt_with_ad(&self.temp_k2, 1, &self.state.h, &mut c)?;
        let rs = parse_key(&c[..len])?;
        self.state.mix_hash(&act[1..50]);
        let temp_k3 = self.state.mix_key(&ecdh(&rs, &self.ephemeral));
        let mut t = act[50..].to_vec();
        decrypt_with_ad(&temp_k3, 0, &self.state.h, &mut t)?;
        let (rk, sk) = self.state.split();
        Ok((rs, Session::new(sk, rk, self.state.ck, self.state.h)))
    }
}

/// One direction's key, nonce and chaining key, rotated every 1,000 uses.
pub struct CipherState {
    key: LessSafeKey,
    k: [u8; 32],
    n: u64,
    ck: [u8; 32],
}

impl std::fmt::Debug for CipherState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CipherState").field("n", &self.n).finish_non_exhaustive()
    }
}

impl CipherState {
    fn new(k: [u8; 32], ck: [u8; 32]) -> Self {
        Self { key: aead_key(&k), k, n: 0, ck }
    }

    /// Uses the current nonce, then advances it and rotates the key at 1,000.
    fn next_nonce(&mut self) -> Nonce {
        let n = nonce(self.n);
        self.n += 1;
        if self.n == KEY_ROTATION_NONCE {
            let (ck, k) = hkdf2(&self.ck, &self.k);
            *self = Self::new(k, ck);
        }
        n
    }

    fn seal(&mut self, buf: &mut Vec<u8>, start: usize) {
        let mut body = buf.split_off(start);
        let n = self.next_nonce();
        self.key
            .seal_in_place_append_tag(n, Aad::empty(), &mut body)
            .expect("message is below the AEAD limit");
        buf.extend_from_slice(&body);
    }

    fn open<'a>(&mut self, ciphertext: &'a mut [u8]) -> Result<&'a mut [u8], NoiseError> {
        let n = self.next_nonce();
        self.key.open_in_place(n, Aad::empty(), ciphertext).map_err(|_| NoiseError::BadTag)
    }
}

/// Encrypts outgoing messages.
#[derive(Debug)]
pub struct Sender {
    cipher: CipherState,
}

impl Sender {
    /// Appends the encrypted length prefix and the encrypted `message` to `out`.
    pub fn encrypt(&mut self, message: &[u8], out: &mut Vec<u8>) {
        assert!(
            message.len() <= MAX_MESSAGE_LEN,
            "message of {} bytes exceeds 65535",
            message.len()
        );
        let start = out.len();
        out.extend_from_slice(&(message.len() as u16).to_be_bytes());
        self.cipher.seal(out, start);
        let start = out.len();
        out.extend_from_slice(message);
        self.cipher.seal(out, start);
    }
}

/// Decrypts incoming messages: first the 18-byte length header, then `length + 16` bytes.
#[derive(Debug)]
pub struct Receiver {
    cipher: CipherState,
}

impl Receiver {
    /// Decrypts a length header, returning the length of the message body that follows (without
    /// its tag).
    pub fn decrypt_length(
        &mut self,
        header: &[u8; LENGTH_HEADER_LEN],
    ) -> Result<usize, NoiseError> {
        let mut buf = *header;
        let plain = self.cipher.open(&mut buf)?;
        Ok(u16::from_be_bytes([plain[0], plain[1]]) as usize)
    }

    /// Decrypts a message body with its trailing tag in place, returning the plaintext.
    pub fn decrypt_body<'a>(&mut self, body: &'a mut [u8]) -> Result<&'a mut [u8], NoiseError> {
        self.cipher.open(body)
    }
}

/// An established session: the two directions' ciphers and the final handshake hash.
#[derive(Debug)]
pub struct Session {
    /// Encrypts what this side sends.
    pub sender: Sender,
    /// Decrypts what this side receives.
    pub receiver: Receiver,
    /// The final handshake digest, unique to this session (channel binding).
    pub handshake_hash: [u8; 32],
}

impl Session {
    fn new(sk: [u8; 32], rk: [u8; 32], ck: [u8; 32], h: [u8; 32]) -> Self {
        Self {
            sender: Sender { cipher: CipherState::new(sk, ck) },
            receiver: Receiver { cipher: CipherState::new(rk, ck) },
            handshake_hash: h,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s = s.trim_start_matches("0x");
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn key(s: &str) -> SecretKey {
        SecretKey::from_byte_array(hex(s).try_into().unwrap()).unwrap()
    }

    const PROLOGUE: &[u8] = b"lightning";
    const RS_PRIV: &str = "2121212121212121212121212121212121212121212121212121212121212121";
    const LS_PRIV: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const IE_PRIV: &str = "1212121212121212121212121212121212121212121212121212121212121212";
    const RE_PRIV: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const ACT_ONE: &str = "0x00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a";
    const ACT_TWO: &str = "0x0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae";
    const ACT_THREE: &str = "0x00b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba";

    fn initiator() -> (Initiator, [u8; 50]) {
        let rs = PublicKey::from_secret_key(SECP256K1, &key(RS_PRIV));
        Initiator::new(key(LS_PRIV), &rs, PROLOGUE, key(IE_PRIV))
    }

    fn responder() -> Responder {
        Responder::new(key(RS_PRIV), PROLOGUE, key(RE_PRIV))
    }

    /// BOLT 8 Appendix A, "transport-initiator successful handshake".
    #[test]
    fn bolt8_initiator_vectors() {
        let (init, act1) = initiator();
        assert_eq!(act1.to_vec(), hex(ACT_ONE));
        let (act3, session) = init.act_two(&hex(ACT_TWO).try_into().unwrap()).unwrap();
        assert_eq!(act3.to_vec(), hex(ACT_THREE));
        assert_eq!(
            session.sender.cipher.k.to_vec(),
            hex("969ab31b4d288cedf6218839b27a3e2140827047f2c0f01bf5c04435d43511a9")
        );
        assert_eq!(
            session.receiver.cipher.k.to_vec(),
            hex("bb9020b8965f4df047e07f955f3c4b88418984aadc5cdb35096b9ea8fa5c3442")
        );
        assert_eq!(
            session.sender.cipher.ck.to_vec(),
            hex("919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01")
        );
    }

    /// BOLT 8 Appendix A, "transport-responder successful handshake".
    #[test]
    fn bolt8_responder_vectors() {
        let (awaiting, act2) = responder().act_one(&hex(ACT_ONE).try_into().unwrap()).unwrap();
        assert_eq!(act2.to_vec(), hex(ACT_TWO));
        let (rs, session) = awaiting.act_three(&hex(ACT_THREE).try_into().unwrap()).unwrap();
        assert_eq!(
            rs.serialize().to_vec(),
            hex("034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa")
        );
        assert_eq!(
            session.receiver.cipher.k.to_vec(),
            hex("969ab31b4d288cedf6218839b27a3e2140827047f2c0f01bf5c04435d43511a9")
        );
        assert_eq!(
            session.sender.cipher.k.to_vec(),
            hex("bb9020b8965f4df047e07f955f3c4b88418984aadc5cdb35096b9ea8fa5c3442")
        );
    }

    /// BOLT 8's initiator and responder failure vectors.
    #[test]
    fn bolt8_failure_vectors() {
        let act2_err = |s: &str| initiator().0.act_two(&hex(s).try_into().unwrap()).err();
        assert_eq!(
            act2_err(
                "0x0102466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae"
            ),
            Some(NoiseError::BadVersion(1))
        );
        assert_eq!(
            act2_err(
                "0x0004466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae"
            ),
            Some(NoiseError::BadPublicKey)
        );
        assert_eq!(
            act2_err(
                "0x0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730af"
            ),
            Some(NoiseError::BadTag)
        );

        let act1_err = |s: &str| responder().act_one(&hex(s).try_into().unwrap()).err();
        assert_eq!(
            act1_err(
                "0x01036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a"
            ),
            Some(NoiseError::BadVersion(1))
        );
        assert_eq!(
            act1_err(
                "0x00046360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a"
            ),
            Some(NoiseError::BadPublicKey)
        );
        assert_eq!(
            act1_err(
                "0x00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6b"
            ),
            Some(NoiseError::BadTag)
        );

        let act3_err = |s: &str| {
            let (awaiting, _) = responder().act_one(&hex(ACT_ONE).try_into().unwrap()).unwrap();
            awaiting.act_three(&hex(s).try_into().unwrap()).err()
        };
        assert_eq!(
            act3_err(
                "0x01b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba"
            ),
            Some(NoiseError::BadVersion(1))
        );
        assert_eq!(
            act3_err(
                "0x00c9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba"
            ),
            Some(NoiseError::BadTag)
        );
        assert_eq!(
            act3_err(
                "0x00bfe3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa2235536ad09a8ee351870c2bb7f78b754a26c6cef79a98d25139c856d7efd252c2ae73c"
            ),
            Some(NoiseError::BadPublicKey)
        );
        assert_eq!(
            act3_err(
                "0x00b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139bb"
            ),
            Some(NoiseError::BadTag)
        );
    }

    /// BOLT 8 "transport-message test": 1,002 "hello" messages across two key rotations.
    #[test]
    fn bolt8_message_vectors() {
        let (init, _) = initiator();
        let (_, mut session) = init.act_two(&hex(ACT_TWO).try_into().unwrap()).unwrap();
        let expected = [
            (0, "0xcf2b30ddf0cf3f80e7c35a6e6730b59fe802473180f396d88a8fb0db8cbcf25d2f214cf9ea1d95"),
            (1, "0x72887022101f0b6753e0c7de21657d35a4cb2a1f5cde2650528bbc8f837d0f0d7ad833b1a256a1"),
            (
                500,
                "0x178cb9d7387190fa34db9c2d50027d21793c9bc2d40b1e14dcf30ebeeeb220f48364f7a4c68bf8",
            ),
            (
                501,
                "0x1b186c57d44eb6de4c057c49940d79bb838a145cb528d6e8fd26dbe50a60ca2c104b56b60e45bd",
            ),
            (
                1000,
                "0x4a2f3cc3b5e78ddb83dcb426d9863d9d9a723b0337c89dd0b005d89f8d3c05c52b76b29b740f09",
            ),
            (
                1001,
                "0x2ecd8c8a5629d0d02ab457a0fdd0f7b90a192cd46be5ecb6ca570bfc5e268338b1a16cf4ef2d36",
            ),
        ];
        let mut next = expected.iter().peekable();
        for i in 0..=1001 {
            let mut out = Vec::new();
            session.sender.encrypt(b"hello", &mut out);
            if let Some((_, want)) = next.next_if(|(n, _)| *n == i) {
                assert_eq!(out, hex(want), "message {i}");
            }
        }
        assert!(next.peek().is_none());
    }

    /// The responder decrypts what the initiator sends across key rotations, and a different
    /// prologue (another chain or layer) fails at Act One.
    #[test]
    fn round_trip_and_prologue_binding() {
        let (init, act1) = initiator();
        let (awaiting, act2) = responder().act_one(&act1).unwrap();
        let (act3, mut a) = init.act_two(&act2).unwrap();
        let (_, mut b) = awaiting.act_three(&act3).unwrap();
        assert_eq!(a.handshake_hash, b.handshake_hash);
        for i in 0..1500u32 {
            let msg = vec![i as u8; (i as usize * 37) % 2000];
            let mut wire = Vec::new();
            a.sender.encrypt(&msg, &mut wire);
            let len = b.receiver.decrypt_length(&wire[..18].try_into().unwrap()).unwrap();
            assert_eq!(len, msg.len());
            let plain = b.receiver.decrypt_body(&mut wire[18..]).unwrap();
            assert_eq!(plain, &msg[..]);
        }
        let other = Responder::new(key(RS_PRIV), b"tp2p other network", key(RE_PRIV));
        assert_eq!(other.act_one(&act1).err(), Some(NoiseError::BadTag));
    }

    /// A replayed message (same ciphertext a second time) fails: the receive nonce has moved on.
    #[test]
    fn replayed_message_is_rejected() {
        let (init, act1) = initiator();
        let (awaiting, act2) = responder().act_one(&act1).unwrap();
        let (act3, mut a) = init.act_two(&act2).unwrap();
        let (_, mut b) = awaiting.act_three(&act3).unwrap();
        let mut wire = Vec::new();
        a.sender.encrypt(b"payload", &mut wire);
        let replay = wire.clone();
        b.receiver.decrypt_length(&wire[..18].try_into().unwrap()).unwrap();
        b.receiver.decrypt_body(&mut wire[18..]).unwrap();
        assert_eq!(
            b.receiver.decrypt_length(&replay[..18].try_into().unwrap()),
            Err(NoiseError::BadTag)
        );
    }
}
