//! discv5 packet target: the masked header, flag and authdata size.
//!
//! tiger's packet decoder is private to discv5.rs, so tiger runs as a real `Discv5` service on a
//! loopback socket in this process and is observed from outside: an ordinary message packet from
//! an unknown node must be answered with WHOAREYOU, anything it drops gets no answer. base-reth's
//! side is sigp `discv5`'s `Packet::decode` with the `basev0` protocol identity: a decoded
//! ordinary message from an unknown node is what its handler challenges with WHOAREYOU; other
//! decoded kinds are dropped without state (no request or challenge outstanding).

use std::{
    net::{SocketAddr, UdpSocket},
    sync::OnceLock,
    time::{Duration, Instant},
};

use aes::cipher::{KeyIvInit, StreamCipher};
use base_tiger_net::{BASE_PROTOCOL_ID, Discv5, Discv5Config, Enr, ForkId};
use discv5::{
    ProtocolIdentity,
    enr::NodeId,
    packet::{Packet, PacketKind},
};

use super::{
    decode::{Seed, Verdict, byte_mutate},
    rng::Rng,
};

/// A running tiger discovery service and a socket to talk to it from.
struct Harness {
    _service: Discv5,
    addr: SocketAddr,
    node_id: [u8; 32],
    socket: UdpSocket,
}

fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let key = secp256k1_tiger::SecretKey::from_byte_array([0x33; 32]).unwrap();
        let service = Discv5::start(Discv5Config {
            secret_key: key,
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            tcp_port: 0,
            external_ip: None,
            fork_id: ForkId { hash: [0; 4], next: 0 },
            bootnodes: Vec::new(),
        })
        .expect("start tiger discv5");
        let enr = Enr::from_text(&service.local_enr_text()).expect("own record");
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
        Harness { addr: service.local_addr(), node_id: enr.node_id().0, _service: service, socket }
    })
}

/// Builds a masked packet: `iv || mask(protocol id, version, flag, nonce, authdata size,
/// authdata) || message`, with each header field taken from the spec.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub(crate) protocol_id: [u8; 6],
    pub(crate) version: u16,
    pub(crate) flag: u8,
    pub(crate) nonce: [u8; 12],
    pub(crate) declared_authdata: u16,
    pub(crate) authdata: Vec<u8>,
    pub(crate) message: Vec<u8>,
    pub(crate) iv: [u8; 16],
}

impl Spec {
    /// The masked packet addressed to `dest`.
    pub(crate) fn encode(&self, dest: &[u8; 32]) -> Vec<u8> {
        let mut header = self.protocol_id.to_vec();
        header.extend_from_slice(&self.version.to_be_bytes());
        header.push(self.flag);
        header.extend_from_slice(&self.nonce);
        header.extend_from_slice(&self.declared_authdata.to_be_bytes());
        header.extend_from_slice(&self.authdata);
        ctr::Ctr128BE::<aes::Aes128>::new(dest[..16].into(), (&self.iv).into())
            .apply_keystream(&mut header);
        let mut out = self.iv.to_vec();
        out.extend(header);
        out.extend_from_slice(&self.message);
        out
    }
}

/// The unmasked spec layout used as fuzz input: protocol id (6) | version (2) | flag (1) |
/// nonce (12) | declared authdata size (2) | actual authdata size (2) | iv (16) | authdata |
/// message.
fn spec_bytes(spec: &Spec) -> Vec<u8> {
    let mut out = spec.protocol_id.to_vec();
    out.extend_from_slice(&spec.version.to_be_bytes());
    out.push(spec.flag);
    out.extend_from_slice(&spec.nonce);
    out.extend_from_slice(&spec.declared_authdata.to_be_bytes());
    out.extend_from_slice(&(spec.authdata.len() as u16).to_be_bytes());
    out.extend_from_slice(&spec.iv);
    out.extend_from_slice(&spec.authdata);
    out.extend_from_slice(&spec.message);
    out
}

fn parse_spec(input: &[u8]) -> Option<Spec> {
    let fixed = input.get(..41)?;
    let actual = u16::from_be_bytes([fixed[23], fixed[24]]) as usize;
    let rest = &input[41..];
    Some(Spec {
        protocol_id: fixed[..6].try_into().unwrap(),
        version: u16::from_be_bytes([fixed[6], fixed[7]]),
        flag: fixed[8],
        nonce: fixed[9..21].try_into().unwrap(),
        declared_authdata: u16::from_be_bytes([fixed[21], fixed[22]]),
        iv: fixed[25..41].try_into().unwrap(),
        authdata: rest.get(..actual.min(rest.len()))?.to_vec(),
        message: rest.get(actual.min(rest.len())..)?.to_vec(),
    })
}

pub(crate) fn generate(rng: &mut Rng) -> Seed {
    let authdata_len = match rng.below(6) {
        0 => *rng.pick(&[0usize, 24, 31, 33, 34, 131, 200]),
        _ => 32,
    };
    let message_len = match rng.below(5) {
        0 => rng.below(20),
        1 => 1200 + rng.below(100),
        _ => 16 + rng.below(100),
    };
    let protocol_id = match rng.below(8) {
        0 => *b"discv5",
        1 => rng.array(),
        _ => BASE_PROTOCOL_ID,
    };
    let spec = Spec {
        protocol_id,
        version: if rng.one_in(8) { *rng.pick(&[0u16, 2, 0x100]) } else { 1 },
        flag: if rng.one_in(6) { *rng.pick(&[1u8, 2, 3, 0xff]) } else { 0 },
        nonce: rng.array(),
        declared_authdata: authdata_len as u16,
        authdata: rng.bytes(authdata_len),
        message: rng.bytes(message_len),
        iv: rng.array(),
    };
    Seed::raw("message-packet", spec_bytes(&spec))
}

pub(crate) fn mutate(rng: &mut Rng, seed: &Seed) -> (Vec<u8>, String) {
    let Some(mut spec) = parse_spec(&seed.bytes) else { return byte_mutate(rng, &seed.bytes) };
    let name = match rng.below(7) {
        0 => {
            spec.declared_authdata =
                spec.declared_authdata.wrapping_add(*rng.pick(&[1u16, 0xffff, 100, 2000]));
            "authdata-size"
        }
        1 => {
            spec.protocol_id = *rng.pick(&[*b"discv5", *b"basev1", *b"Basev0"]);
            "protocol-id"
        }
        2 => {
            spec.flag = rng.next_u64() as u8;
            "flag"
        }
        3 => {
            let len = *rng.pick(&[0usize, 1, 15, 1280 - 55, 1280 - 54, 1300]);
            spec.message = rng.bytes(len);
            "total-size"
        }
        4 => {
            spec.version = rng.next_u64() as u16;
            "version"
        }
        _ => return byte_mutate(rng, &seed.bytes),
    };
    (spec_bytes(&spec), name.to_string())
}

fn tiger(packet: &[u8]) -> Verdict {
    let h = harness();
    let mut buf = [0u8; 2048];
    while h.socket.recv_from(&mut buf).is_ok() {}
    let _ = h.socket.send_to(packet, h.addr);
    let deadline = Instant::now() + Duration::from_millis(60);
    while Instant::now() < deadline {
        if let Ok((len, _)) = h.socket.recv_from(&mut buf) {
            return Verdict::ok(format!("answered ({len} bytes)"));
        }
    }
    Verdict::err("silent")
}

fn reth(packet: &[u8]) -> Verdict {
    let h = harness();
    let identity =
        ProtocolIdentity { protocol_id: BASE_PROTOCOL_ID, protocol_version: 1u16.to_be_bytes() };
    match Packet::decode(&NodeId::new(&h.node_id), identity, packet) {
        Ok((packet, _)) => match packet.header.kind {
            PacketKind::Message { .. } => Verdict::ok("answered"),
            other => Verdict::err(format!("silent: decoded {other:?}")),
        },
        Err(e) => Verdict::err(e),
    }
}

pub(crate) fn check(_case: &str, input: &[u8]) -> (Verdict, Verdict) {
    let Some(spec) = parse_spec(input) else { return (Verdict::err("spec"), Verdict::err("spec")) };
    let packet = spec.encode(&harness().node_id);
    let (t, r) = (tiger(&packet), reth(&packet));
    // Normalize: compare answered against silent only.
    let norm = |v: Verdict| match v {
        Verdict::Accept(_) => Verdict::ok("answered"),
        other => other,
    };
    (norm(t), norm(r))
}
