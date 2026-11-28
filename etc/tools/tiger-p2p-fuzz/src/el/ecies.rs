//! ECIES target: the EIP-8 auth and ack packets and the `RLPx` frame header and MAC.
//!
//! The harness is the initiator (wire.rs): it builds auth bodies as RLP trees and seals them
//! itself, so body mutations still carry a valid MAC and reach the parsers. tiger's side is
//! `accept` (recipient) and `Initiator::finish` (ack); base-reth's is `ECIES::read_auth` /
//! `read_ack` with the size prefix framing of `ECIESCodec`. For frames, the harness completes a
//! real handshake with each side, then sends both the same frame plaintext under each session's
//! keys and compares what `FrameIngress` and `ECIES::read_header` / `read_body` make of it.

use alloy_primitives::{B512, hex};
use base_tiger_net::{FrameCodec, PeerId as TigerPeerId};
use bytes::BytesMut;
use reth_ecies::algorithm::ECIES;
use secp256k1::{PublicKey, SECP256K1, SecretKey};

use super::{
    decode::{Seed, Verdict, byte_mutate},
    rlp::{self, Item, b, int, l},
    rng::Rng,
    wire::{self, Handshake, Seal, header_plain, node_id},
};

/// The fixed static keys: the harness (initiator) and the recipient under test.
fn keys() -> (SecretKey, SecretKey) {
    let a = SecretKey::from_byte_array(&[0x11; 32]).unwrap();
    let b = SecretKey::from_byte_array(&[0x22; 32]).unwrap();
    (a, b)
}

fn tiger_key(key: &SecretKey) -> secp256k1_tiger::SecretKey {
    secp256k1_tiger::SecretKey::from_byte_array(key.secret_bytes()).unwrap()
}

/// reth's limit on the first frame after the handshake (`MAX_INITIAL_HANDSHAKE_SIZE`).
const MAX_INITIAL_FRAME: usize = 2048;

/// An IV whose low 64 bits wrap after two blocks, where 64- and 128-bit CTR counters diverge.
const WRAPPING_IV: [u8; 16] =
    [0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe];

/// aux layout for auth/ack seeds: padding (2 bytes, big-endian), then the body encoding.
fn aux(padding: usize, body: &[u8]) -> Vec<u8> {
    let mut out = (padding as u16).to_be_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

pub(crate) fn generate(rng: &mut Rng) -> Seed {
    let (a, b_key) = keys();
    let b_pub = b_key.public_key(SECP256K1);
    let a_pub = a.public_key(SECP256K1);
    let padding = match rng.below(6) {
        0 => 0,
        1 => 1900 + rng.below(200),
        _ => 100 + rng.below(201),
    };
    match rng.below(4) {
        0 | 1 => {
            let hs = Handshake::new(rng, a);
            let mut body = hs.auth_body(&b_pub, 4);
            if rng.one_in(5) {
                // EIP-8: extra list elements must be ignored.
                if let Item::List(items) = &mut body.item {
                    items.push(int(rng.edge_u64()));
                }
            }
            let packet =
                wire::seal_packet(rng, &b_pub, &body.encode(), Seal { padding, ..Seal::default() });
            Seed {
                case: "auth".into(),
                tree: None,
                bytes: packet,
                aux: aux(padding, &body.encode()),
            }
        }
        2 => {
            let eph = wire::secret(rng).public_key(SECP256K1);
            let body = l(vec![b(node_id(&eph).to_vec()), b(rng.bytes(32)), int(4)]);
            let packet =
                wire::seal_packet(rng, &a_pub, &body.encode(), Seal { padding, ..Seal::default() });
            Seed {
                case: "ack".into(),
                tree: None,
                bytes: packet,
                aux: aux(padding, &body.encode()),
            }
        }
        _ => {
            // Frame spec: header plaintext (16) | body length (2) | corruption (1) | position (2)
            // | first (1) | body.
            let len = *rng.pick(&[1usize, 2, 15, 16, 17, 100, 2047, 2048, 2049, 3000, 0xffff]);
            let declared = match rng.below(6) {
                0 => 0,
                1 => len + 1,
                2 => len.saturating_sub(1),
                3 => *rng.pick(&[2049usize, 4096, 0xff_ffff]),
                _ => len,
            };
            let mut header = header_plain(declared);
            match rng.below(5) {
                0 => header[3..6].copy_from_slice(&[0xc0, 0x00, 0x00]),
                1 => header[3..16].copy_from_slice(&rng.bytes(13)),
                _ => {}
            }
            let corruption = if rng.one_in(2) { 0 } else { rng.below(7) as u8 };
            let mut spec = header.to_vec();
            spec.extend_from_slice(&(len as u16).to_be_bytes());
            spec.push(corruption);
            spec.extend_from_slice(&(rng.below(0xffff) as u16).to_be_bytes());
            spec.push(u8::from(rng.one_in(2)));
            spec.extend(rng.bytes(len));
            let case = if spec[21] == 1 { "frame-first" } else { "frame-next" };
            Seed::raw(case, spec)
        }
    }
}

pub(crate) fn mutate(rng: &mut Rng, seed: &Seed) -> (Vec<u8>, String) {
    if seed.case.starts_with("frame") {
        return byte_mutate(rng, &seed.bytes);
    }
    let (a, b_key) = keys();
    let remote =
        if seed.case == "auth" { b_key.public_key(SECP256K1) } else { a.public_key(SECP256K1) };
    let padding = u16::from_be_bytes([seed.aux[0], seed.aux[1]]) as usize;
    let body = rlp::parse(&seed.aux[2..]).unwrap().0;
    let seal = |rng: &mut Rng, body: &[u8], seal: Seal| wire::seal_packet(rng, &remote, body, seal);
    let (out, name) = match rng.below(9) {
        0..=3 => {
            let (mutated, name) = rlp::mutate(rng, &body);
            return (
                seal(rng, &mutated, Seal { padding, ..Seal::default() }),
                format!("body:{name}"),
            );
        }
        4 => {
            let delta = *rng.pick(&[-1i32, 1, 16, -16, 0x8000]);
            (
                seal(rng, &body.encode(), Seal { padding, prefix_delta: delta, iv: None }),
                "size-prefix",
            )
        }
        5 => (
            seal(rng, &body.encode(), Seal { padding, prefix_delta: 0, iv: Some(WRAPPING_IV) }),
            "iv-low64-wrap",
        ),
        6 => {
            let padding = *rng.pick(&[0, 1, 2100]);
            (seal(rng, &body.encode(), Seal { padding, ..Seal::default() }), "padding")
        }
        7 => {
            // Corrupt the ciphertext, MAC or ephemeral key while keeping the length.
            let mut out = seed.bytes.clone();
            let region = rng.below(3);
            let at = match region {
                0 => 2 + rng.below(65),
                1 => out.len() - 1 - rng.below(32),
                _ => 2 + 81 + rng.below(out.len() - 2 - 81 - 32),
            };
            out[at] ^= 1 << rng.below(8);
            (out, ["corrupt-ephemeral", "corrupt-mac", "corrupt-ciphertext"][region])
        }
        _ => return byte_mutate(rng, &seed.bytes),
    };
    (out, name.to_string())
}

/// What each reader does with a byte stream starting with a handshake packet.
fn read_packet(input: &[u8]) -> Result<&[u8], Verdict> {
    if input.len() < 2 {
        return Err(Verdict::code("incomplete", "prefix"));
    }
    let total = u16::from_be_bytes([input[0], input[1]]) as usize + 2;
    input.get(..total).ok_or_else(|| Verdict::code("incomplete", total))
}

fn check_auth(input: &[u8]) -> (Verdict, Verdict) {
    let (_, b_key) = keys();
    let tiger = (|| {
        if input.len() >= 2 {
            base_tiger_net::handshake_packet_len([input[0], input[1]]).map_err(Verdict::err)?;
        }
        let packet = read_packet(input)?;
        let accepted =
            base_tiger_net::ecies_accept(&tiger_key(&b_key), packet).map_err(Verdict::err)?;
        Ok(Verdict::ok(format!("remote={}", accepted.remote)))
    })()
    .unwrap_or_else(|v| v);
    let reth = (|| {
        let packet = read_packet(input)?;
        let mut ecies = ECIES::new_server(b_key).map_err(Verdict::err)?;
        ecies.read_auth(&mut packet.to_vec()).map_err(Verdict::err)?;
        Ok(Verdict::ok(format!("remote={}", ecies.remote_id())))
    })()
    .unwrap_or_else(|v| v);
    (tiger, reth)
}

fn check_ack(input: &[u8]) -> (Verdict, Verdict) {
    let (a, b_key) = keys();
    let b_id = node_id(&b_key.public_key(SECP256K1));
    let tiger = (|| {
        if input.len() >= 2 {
            base_tiger_net::handshake_packet_len([input[0], input[1]]).map_err(Verdict::err)?;
        }
        let packet = read_packet(input)?;
        let initiator = base_tiger_net::Initiator::new(&tiger_key(&a), &TigerPeerId::from(b_id))
            .map_err(Verdict::err)?;
        initiator.finish(&tiger_key(&a), packet).map_err(Verdict::err)?;
        Ok(Verdict::ok("ack accepted"))
    })()
    .unwrap_or_else(|v| v);
    let reth = (|| {
        let packet = read_packet(input)?;
        let mut ecies = ECIES::new_client(a, B512::from(b_id)).map_err(Verdict::err)?;
        let mut auth = BytesMut::new();
        ecies.write_auth(&mut auth);
        ecies.read_ack(&mut packet.to_vec()).map_err(Verdict::err)?;
        Ok(Verdict::ok("ack accepted"))
    })()
    .unwrap_or_else(|v| v);
    (tiger, reth)
}

/// Builds the frame bytes from a spec under `writer`.
fn build_frame(writer: &mut wire::FrameWriter, spec: &[u8]) -> Option<Vec<u8>> {
    let header: [u8; 16] = spec.get(..16)?.try_into().ok()?;
    let len = u16::from_be_bytes([*spec.get(16)?, *spec.get(17)?]) as usize;
    let corruption = *spec.get(18)?;
    let position = u16::from_be_bytes([*spec.get(19)?, *spec.get(20)?]) as usize;
    let body = spec.get(22..)?;
    let body = &body[..len.min(body.len())];
    let mut frame = writer.header(header);
    frame.extend(writer.body(body));
    let n = frame.len();
    match corruption {
        1 => frame[position % 16] ^= 1,
        2 => frame[16 + position % 16] ^= 1,
        3 if n > 48 => frame[32 + position % (n - 48)] ^= 1,
        4 => frame[n - 1 - position % 16] ^= 1,
        5 => frame.truncate(32 + position % (n - 31)),
        6 => frame.extend_from_slice(&[0u8; 16]),
        _ => {}
    }
    Some(frame)
}

/// The first, valid frame of a `frame-next` case.
const WARMUP: &[u8] = b"\x80tiger warmup";

fn tiger_frames(
    hs: &Handshake,
    b_key: &SecretKey,
    spec: &[u8],
    first: bool,
    rng: &mut Rng,
) -> Verdict {
    let auth = wire::seal_packet(
        rng,
        &b_key.public_key(SECP256K1),
        &hs.auth_body(&b_key.public_key(SECP256K1), 4).encode(),
        Seal::default(),
    );
    let Ok(accepted) = base_tiger_net::ecies_accept(&tiger_key(b_key), &auth) else {
        return Verdict::err("handshake");
    };
    let Some((mut writer, _)) = hs.finish(&auth, &accepted.ack) else {
        return Verdict::err("handshake");
    };
    let mut ingress = accepted.codec.ingress;
    let mut read = |bytes: &[u8]| -> Result<Vec<u8>, Verdict> {
        let header: [u8; 32] = bytes
            .get(..32)
            .ok_or_else(|| Verdict::code("incomplete", "header"))?
            .try_into()
            .unwrap();
        let mut header = header;
        let size = ingress.decode_header(&mut header).map_err(Verdict::err)?;
        let need = FrameCodec::body_len(size);
        let mut body =
            bytes.get(32..32 + need).ok_or_else(|| Verdict::code("incomplete", need))?.to_vec();
        ingress.decode_body(&mut body, size).map(<[u8]>::to_vec).map_err(Verdict::err)
    };
    if !first {
        let warm = writer.frame(WARMUP);
        if let Err(v) = read(&warm) {
            return v;
        }
    }
    let Some(frame) = build_frame(&mut writer, spec) else { return Verdict::err("spec") };
    match read(&frame) {
        Ok(payload) => Verdict::ok(hex::encode(payload)),
        Err(v) => v,
    }
}

fn reth_frames(
    hs: &Handshake,
    b_key: &SecretKey,
    spec: &[u8],
    first: bool,
    rng: &mut Rng,
) -> Verdict {
    let b_pub: PublicKey = b_key.public_key(SECP256K1);
    let auth = wire::seal_packet(rng, &b_pub, &hs.auth_body(&b_pub, 4).encode(), Seal::default());
    let Ok(mut ecies) = ECIES::new_server(*b_key) else { return Verdict::err("server") };
    if ecies.read_auth(&mut auth.clone()).is_err() {
        return Verdict::err("handshake");
    }
    let mut ack = BytesMut::new();
    ecies.write_ack(&mut ack);
    let Some((mut writer, _)) = hs.finish(&auth, &ack) else { return Verdict::err("handshake") };
    let mut read = |bytes: &[u8], initial: bool| -> Result<Vec<u8>, Verdict> {
        let mut header =
            bytes.get(..32).ok_or_else(|| Verdict::code("incomplete", "header"))?.to_vec();
        let size = ecies.read_header(&mut header).map_err(Verdict::err)?;
        if initial && size > MAX_INITIAL_FRAME {
            return Err(Verdict::err("InitialHeaderBodyTooLarge"));
        }
        let need = ecies.body_len();
        let mut body =
            bytes.get(32..32 + need).ok_or_else(|| Verdict::code("incomplete", need))?.to_vec();
        ecies.read_body(&mut body).map(|b| b.to_vec()).map_err(Verdict::err)
    };
    if !first {
        let warm = writer.frame(WARMUP);
        if let Err(v) = read(&warm, true) {
            return v;
        }
    }
    let Some(frame) = build_frame(&mut writer, spec) else { return Verdict::err("spec") };
    match read(&frame, first) {
        Ok(payload) => Verdict::ok(hex::encode(payload)),
        Err(v) => v,
    }
}

fn check_frame(case: &str, spec: &[u8]) -> (Verdict, Verdict) {
    let (a, b_key) = keys();
    // The handshake randomness is fixed per input, so a case replays identically.
    let mut rng =
        Rng::new(spec.iter().fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(*b as u64)));
    let hs = Handshake::new(&mut rng, a);
    let first = case == "frame-first";
    let tiger = tiger_frames(&hs, &b_key, spec, first, &mut rng.clone());
    let reth = reth_frames(&hs, &b_key, spec, first, &mut rng);
    (tiger, reth)
}

pub(crate) fn check(case: &str, input: &[u8]) -> (Verdict, Verdict) {
    match case {
        "auth" => check_auth(input),
        "ack" => check_ack(input),
        _ => check_frame(case, input),
    }
}
