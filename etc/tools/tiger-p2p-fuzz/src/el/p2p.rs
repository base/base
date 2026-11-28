//! `p2p` capability target: Hello (decode, version check, capability negotiation), Disconnect,
//! Ping and Pong, as each side processes them off the wire.
//!
//! tiger: `Hello::decode` and `negotiate` (rlpx.rs `hello`), and for later messages the frame
//! payload is Snappy-decompressed first (`RlpxReader::poll`), then `P2pMessage::decode`.
//! base-reth: `UnauthedP2PStream::handshake` (`P2PMessage::decode`, the `protocol_version as u8`
//! equality, `SharedCapabilities::try_new`) and `P2PStream::poll_next` for later messages (raw
//! `DisconnectReason::decode` first, then Snappy; Ping and Pong payloads validated).

use alloy_primitives::hex;
use alloy_rlp::Decodable;
use base_tiger_net::{
    Capability as TigerCapability, ETH_CAPABILITIES, Hello, P2pMessage, RawMessage,
};
use reth_eth_wire::{
    DisconnectReason, EthVersion, HelloMessage, P2PMessage, ProtocolVersion,
    capability::{SharedCapabilities, SharedCapability},
    protocol::Protocol,
};

use super::{
    decode::{Seed, Verdict},
    rlp::{b, int, l},
    rng::Rng,
    snappy::MAX_PAYLOAD,
};

/// reth's limit on the first frame after the ECIES handshake (`MAX_INITIAL_HANDSHAKE_SIZE`).
const MAX_INITIAL_FRAME: usize = 2048;

pub(crate) fn generate(rng: &mut Rng) -> Seed {
    match rng.below(4) {
        0 | 1 => {
            let version = *rng.pick(&[5u64, 5, 5, 4, 6, 0, 0x105, 55]);
            let client = if rng.one_in(10) { vec![b'a'; 2100] } else { b"tiger-fuzz/v1".to_vec() };
            let n = rng.below(5);
            let mut caps = Vec::new();
            for _ in 0..n {
                let (name, version): (&[u8], u64) = match rng.below(6) {
                    0 => (b"snap", 1),
                    1 => (b"les", 4),
                    _ => (b"eth", *rng.pick(&[66u64, 67, 68, 69, 69, 70, 71, 72, 65])),
                };
                caps.push(l(vec![b(name.to_vec()), int(version)]));
            }
            if rng.one_in(3) {
                caps.push(l(vec![b(b"eth".to_vec()), int(69)]));
            }
            let mut fields = vec![
                int(version),
                b(client),
                l(caps),
                int(rng.edge_u64() & 0xffff),
                b(rng.bytes(64)),
            ];
            if rng.one_in(6) {
                // EIP-8: later versions may append fields.
                fields.push(b(b"future".to_vec()));
            }
            Seed::tree("hello", l(fields))
        }
        2 => {
            let reason = *rng.pick(&[0u8, 1, 2, 3, 4, 8, 0x0b, 0x10, 0x11, 0xff]);
            let payload = alloy_rlp::encode(vec![reason]);
            let wire = match rng.below(3) {
                0 => base_tiger_net::snappy_compress(&payload),
                1 => payload,
                _ => vec![reason],
            };
            Seed::raw("disconnect", wire)
        }
        _ => {
            let case = if rng.one_in(2) { "ping" } else { "pong" };
            Seed::raw(case, base_tiger_net::snappy_compress(&[0xc0]))
        }
    }
}

fn tiger_hello(input: &[u8]) -> Verdict {
    let hello = match Hello::decode(&mut &input[..]) {
        Ok(hello) => hello,
        Err(e) => return Verdict::err(e),
    };
    let caps: Vec<String> =
        hello.capabilities.iter().map(|c| format!("{}/{}", c.name, c.version)).collect();
    let remote: Vec<TigerCapability> = hello.capabilities.clone();
    let shared = match base_tiger_net::negotiate(&ETH_CAPABILITIES, &remote) {
        Ok(shared) => shared
            .iter()
            .map(|s| format!("{}/{}@{}", s.name, s.version, s.offset))
            .collect::<Vec<_>>(),
        Err(e) => return Verdict::code("useless-peer", e),
    };
    Verdict::ok(format!(
        "v={} client={} caps={caps:?} port={} id={} shared={shared:?}",
        hello.protocol_version,
        hex::encode(hello.client_id.as_bytes()),
        hello.port,
        hello.id
    ))
}

fn reth_hello(input: &[u8]) -> Verdict {
    if input.len() + 1 > MAX_INITIAL_FRAME {
        return Verdict::err("InitialHeaderBodyTooLarge");
    }
    // The message id is RLP: Hello's id 0 travels as 0x80.
    let mut wire = vec![0x80];
    wire.extend_from_slice(input);
    let hello: HelloMessage = match P2PMessage::decode(&mut &wire[..]) {
        Ok(P2PMessage::Hello(hello)) => hello,
        Ok(other) => return Verdict::err(format!("not hello: {other:?}")),
        Err(e) => return Verdict::err(e),
    };
    if hello.protocol_version as u8 != ProtocolVersion::V5 as u8 {
        return Verdict::code("incompatible-version", hello.protocol_version);
    }
    let caps: Vec<String> =
        hello.capabilities.iter().map(|c| format!("{}/{}", c.name, c.version)).collect();
    let local: Vec<Protocol> = EthVersion::ALL_VERSIONS.iter().copied().map(Into::into).collect();
    let shared = match SharedCapabilities::try_new(local, hello.capabilities.clone()) {
        Ok(shared) => shared
            .iter_caps()
            .map(|c| match c {
                SharedCapability::Eth { version, offset } => {
                    format!("eth/{}@{offset}", *version as u8)
                }
                SharedCapability::UnknownCapability { cap, offset, .. } => {
                    format!("{cap}@{offset}")
                }
            })
            .collect::<Vec<_>>(),
        Err(e) => return Verdict::code("useless-peer", e),
    };
    Verdict::ok(format!(
        "v={} client={} caps={caps:?} port={} id={} shared={shared:?}",
        hello.protocol_version as u8,
        hex::encode(hello.client_version.as_bytes()),
        hello.port,
        hello.id
    ))
}

/// tiger: decompress the frame payload, then classify the message.
fn tiger_message(id: u8, input: &[u8]) -> Verdict {
    let payload = match base_tiger_net::snappy_decompress(input, MAX_PAYLOAD) {
        Ok(p) => p,
        Err(e) => return Verdict::err(format!("bad snappy {e:?}")),
    };
    match P2pMessage::decode(&RawMessage { id, payload }) {
        Ok(Some(P2pMessage::Disconnect(reason))) => Verdict::ok(format!("disconnect {reason}")),
        Ok(Some(P2pMessage::Ping)) => Verdict::ok("pong sent"),
        Ok(Some(P2pMessage::Pong)) => Verdict::ok("pong"),
        Ok(None) => Verdict::err("capability message"),
        Err(e) => Verdict::err(e),
    }
}

/// base-reth: `P2PStream::poll_next` up to the point the message is handled.
fn reth_message(id: u8, input: &[u8]) -> Verdict {
    if id == 0x01
        && let Ok(reason) = DisconnectReason::decode(&mut &input[..])
    {
        return Verdict::ok(format!("disconnect {}", reason as u8));
    }
    if id == 0x02 || id == 0x03 {
        let ok = snap::raw::decompress_len(input).ok() == Some(1)
            && snap::raw::Decoder::new().decompress_vec(input).ok().as_deref() == Some(&[0xc0][..]);
        if !ok {
            return Verdict::err("InvalidPingPongPayload");
        }
        return Verdict::ok(if id == 0x02 { "pong sent" } else { "pong" });
    }
    let len = match snap::raw::decompress_len(input) {
        Ok(len) => len,
        Err(e) => return Verdict::err(e),
    };
    if len > MAX_PAYLOAD {
        return Verdict::err("MessageTooBig");
    }
    let mut out = vec![0u8; len];
    if let Err(e) = snap::raw::Decoder::new().decompress(input, &mut out) {
        return Verdict::err(e);
    }
    match DisconnectReason::decode(&mut &out[..]) {
        Ok(reason) => Verdict::ok(format!("disconnect {}", reason as u8)),
        Err(e) => Verdict::err(e),
    }
}

pub(crate) fn check(case: &str, input: &[u8]) -> (Verdict, Verdict) {
    match case {
        "hello" => (tiger_hello(input), reth_hello(input)),
        _ => {
            let id = match case {
                "disconnect" => 0x01,
                "ping" => 0x02,
                _ => 0x03,
            };
            (tiger_message(id, input), reth_message(id, input))
        }
    }
}
