//! tp2p fuzzing: message decoding, chunk reassembly and the Noise XK handshake in process
//! (`wire`, `handshake`), and attacking peers against a live tp2p node over loopback (`live`).
//!
//! In process, every input is a mutation of a valid encoding. A decoder must not panic, must
//! refuse what it cannot re-encode byte for byte (one encoding per message, so a message's id and
//! its forwarded bytes agree on every node), and the reassembler must never hold more memory
//! than the bytes it was given plus one chunk (by construction; checked here only for panics).
//!
//! Live, each attack runs against a fresh node while an honest peer checks that the node still
//! completes handshakes and answers; the attack must end in a disconnect within the stated
//! deadline, and the process's resident memory must stay bounded.

use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    sync::mpsc,
    time::{Duration, Instant},
};

use base_tiger_p2p::{
    ACT_ONE_LEN, ACT_TWO_LEN, Config, DisconnectReason, Event, FLAG_BULK, FLAG_END, FLAG_START,
    HANDSHAKE_TIMEOUT, Handle, Initiator, LENGTH_HEADER_LEN, MAX_IDS,
    MAX_PENDING_HANDSHAKES_PER_IP, MESSAGE_TIMEOUT, Message, MessageId, Network, PeerRecord,
    Reassembler, RequestKind, Responder, ResponseCode, Session, Status, chunk, start,
};
use secp256k1_tiger::{PublicKey, SECP256K1, SecretKey};

/// `SplitMix64`, so every case reproduces from `(seed, index)`.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }

    fn secret(&mut self) -> SecretKey {
        loop {
            let mut bytes = [0u8; 32];
            bytes.iter_mut().for_each(|b| *b = self.next() as u8);
            if let Ok(key) = SecretKey::from_byte_array(bytes) {
                return key;
            }
        }
    }
}

const NETWORK: Network = Network { chain_id: 8453, genesis: [7; 32], layer: Network::EXECUTION };

/// `tp2p <wire|handshake|live> [--seed S] [--iters N]`.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let mut seed = 1u64;
    let mut iters = 100_000u64;
    let mut index = 1;
    while index < args.len() {
        let value = args.get(index + 1).ok_or_else(|| format!("{} needs a value", args[index]))?;
        match args[index].as_str() {
            "--seed" => seed = value.parse().map_err(|e| format!("--seed: {e}"))?,
            "--iters" => iters = value.parse().map_err(|e| format!("--iters: {e}"))?,
            other => return Err(format!("unknown flag {other}")),
        }
        index += 2;
    }
    match args.first().map(String::as_str) {
        Some("wire") => wire(seed, iters),
        Some("handshake") => handshake(seed, iters),
        Some("live") => live(seed),
        _ => Err("usage: tp2p <wire|handshake|live> [--seed S] [--iters N]".into()),
    }
}

fn random_message(rng: &mut Rng, signer: &SecretKey) -> Message {
    let small = |rng: &mut Rng| {
        let n = rng.below(64);
        rng.bytes(n)
    };
    let ids = |rng: &mut Rng| -> Vec<MessageId> {
        (0..rng.below(8)).map(|_| rng.bytes(20).try_into().expect("20 bytes")).collect()
    };
    match rng.below(13) {
        0 => Message::Status(Status {
            version: 1,
            capabilities: rng.next() as u32,
            head_number: rng.next(),
            head_hash: rng.bytes(32).try_into().expect("32 bytes"),
            listen_port: rng.next() as u16,
        }),
        1 => Message::Ping(rng.next()),
        2 => Message::Pong(rng.next()),
        3 => Message::Disconnect(DisconnectReason::Timeout),
        4 => Message::GetPeers(rng.next() as u16),
        5 => {
            let records = (0..rng.below(3))
                .map(|_| {
                    let addr = if rng.below(2) == 0 {
                        SocketAddr::from(([10, 0, rng.next() as u8, 1], rng.next() as u16))
                    } else {
                        SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, rng.next() as u16], 30303))
                    };
                    PeerRecord::signed(signer, &[7; 32], addr, rng.next(), rng.next() as u32)
                })
                .collect();
            Message::Peers(records)
        }
        6 => Message::Request { id: rng.next() as u32, kind: rng.next() as u16, body: small(rng) },
        7 => {
            let code = [ResponseCode::Ok, ResponseCode::NotFound, ResponseCode::Busy][rng.below(3)];
            let body = if code == ResponseCode::Ok { small(rng) } else { Vec::new() };
            Message::Response { id: rng.next() as u32, code, body }
        }
        8 => Message::Gossip { topic: rng.next() as u8, data: small(rng) },
        9 => Message::IHave(ids(rng)),
        10 => Message::IWant(ids(rng)),
        11 => Message::IDontWant(ids(rng)),
        _ => Message::Notify { kind: rng.next() as u16, body: small(rng) },
    }
}

fn mutate(rng: &mut Rng, input: &mut Vec<u8>) {
    for _ in 0..1 + rng.below(4) {
        match rng.below(6) {
            0 if !input.is_empty() => {
                let i = rng.below(input.len());
                input[i] ^= 1 << rng.below(8);
            }
            1 if !input.is_empty() => {
                let i = rng.below(input.len());
                input[i] = [0, 0xff, 0x7f, 0x80, 1][rng.below(5)];
            }
            2 => input.truncate(rng.below(input.len() + 1)),
            3 => {
                let n = 1 + rng.below(8);
                input.extend(rng.bytes(n));
            }
            4 if input.len() >= 4 => {
                // A count or length field near the front set huge.
                let i = rng.below(input.len().min(8) - 3);
                input[i..i + 4].copy_from_slice(&u32::MAX.to_be_bytes());
            }
            _ => {
                let i = rng.below(input.len() + 1);
                input.insert(i, rng.next() as u8);
            }
        }
    }
}

/// Message decoding and chunk reassembly.
fn wire(seed: u64, iters: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let signer = rng.secret();
    let (mut accepted, mut non_canonical) = (0u64, 0u64);
    for case in 0..iters {
        let message = random_message(&mut rng, &signer);
        let type_byte = message.type_byte();
        let body = message.encode_body();
        if Message::decode(type_byte, &body).as_ref() != Ok(&message) {
            return Err(format!("case {case}: a valid {message:?} does not round-trip"));
        }
        let mut input = body.clone();
        mutate(&mut rng, &mut input);
        let type_byte = if rng.below(16) == 0 { rng.next() as u8 } else { type_byte };
        if let Ok(decoded) = Message::decode(type_byte, &input) {
            accepted += 1;
            if decoded.encode_body() != input || decoded.type_byte() != type_byte {
                non_canonical += 1;
                eprintln!(
                    "tp2p wire: non-canonical encoding accepted: type {type_byte:#04x} input {} decoded {decoded:?}",
                    alloy_primitives::hex::encode(&input)
                );
            }
        }
        reassembly(&mut rng, case, type_byte, &body)?;
    }
    println!(
        "tp2p wire: {iters} cases, {accepted} mutated inputs decoded, {non_canonical} non-canonical"
    );
    if non_canonical > 0 {
        Err(format!("{non_canonical} non-canonical encodings accepted"))
    } else {
        Ok(())
    }
}

/// Chunks `body`, mutates the chunk stream, and checks the reassembler's answers and memory.
fn reassembly(rng: &mut Rng, case: u64, type_byte: u8, body: &[u8]) -> Result<(), String> {
    let bulk = rng.below(2) == 0;
    let mut chunks = Vec::new();
    chunk(type_byte, body, bulk, |c| chunks.push(c));
    let limit = |_: u8| 1 << 20;
    let mut clean = Reassembler::default();
    let mut out = None;
    for c in &chunks {
        out = clean
            .push(c, limit)
            .map_err(|e| format!("case {case}: a valid chunk stream fails: {e:?}"))?;
    }
    if out != Some((type_byte, body.to_vec())) {
        return Err(format!("case {case}: chunk round trip lost the message"));
    }
    // A hostile stream: mutated chunks, reordered, duplicated, with huge declared lengths.
    let mut reassembler = Reassembler::default();
    for _ in 0..1 + rng.below(6) {
        let mut c = match rng.below(4) {
            0 => {
                let mut start = vec![FLAG_START | if bulk { FLAG_BULK } else { 0 }, type_byte];
                start.extend_from_slice(&(rng.next() as u32).to_be_bytes());
                let n = rng.below(64);
                start.extend(rng.bytes(n));
                start
            }
            1 => vec![FLAG_END, 0, 0],
            _ => chunks[rng.below(chunks.len())].clone(),
        };
        mutate(rng, &mut c);
        if reassembler.push(&c, limit).is_err() {
            break;
        }
    }
    Ok(())
}

/// The Noise XK handshake under mutated acts and frames.
fn handshake(seed: u64, iters: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let prologue = NETWORK.prologue();
    let (mut refused, mut bad_accepted) = (0u64, 0u64);
    for case in 0..iters {
        let (initiator_key, responder_key) = (rng.secret(), rng.secret());
        let responder_pub = PublicKey::from_secret_key(SECP256K1, &responder_key);
        let (initiator, act_one) =
            Initiator::new(initiator_key, &responder_pub, &prologue, rng.secret());
        let responder = Responder::new(responder_key, &prologue, rng.secret());
        let step = rng.below(4);
        let mut one = act_one;
        if step == 0 {
            flip(&mut rng, &mut one);
        }
        let Ok((awaiting, act_two)) = responder.act_one(&one) else {
            refused += 1;
            continue;
        };
        bad_accepted += u64::from(step == 0);
        let mut two = act_two;
        if step == 1 {
            flip(&mut rng, &mut two);
        }
        let Ok((act_three, mut initiator_session)) = initiator.act_two(&two) else {
            refused += 1;
            continue;
        };
        bad_accepted += u64::from(step == 1);
        let mut three = act_three;
        if step == 2 {
            flip(&mut rng, &mut three);
        }
        let Ok((remote, mut responder_session)) = awaiting.act_three(&three) else {
            refused += 1;
            continue;
        };
        bad_accepted += u64::from(step == 2);
        if remote != PublicKey::from_secret_key(SECP256K1, &initiator_key) {
            return Err(format!("case {case}: the responder learned the wrong initiator key"));
        }
        // Frames: one valid, then one tampered, which must fail.
        let n = rng.below(256);
        let message = rng.bytes(n);
        let mut wire = Vec::new();
        initiator_session.sender.encrypt(&message, &mut wire);
        if step == 3 {
            flip(&mut rng, &mut wire);
        }
        match open(&mut responder_session, &wire) {
            Ok(plain) if plain == message => bad_accepted += u64::from(step == 3),
            Ok(_) => return Err(format!("case {case}: a frame decrypted to other bytes")),
            Err(()) if step == 3 => refused += 1,
            Err(()) => return Err(format!("case {case}: a valid frame failed to decrypt")),
        }
    }
    println!(
        "tp2p handshake: {iters} cases, {refused} tampered inputs refused, {bad_accepted} accepted"
    );
    if bad_accepted > 0 {
        Err(format!("{bad_accepted} tampered handshake inputs accepted"))
    } else {
        Ok(())
    }
}

fn flip(rng: &mut Rng, bytes: &mut [u8]) {
    let i = rng.below(bytes.len());
    bytes[i] ^= 1 << (rng.next() % 8);
}

fn open(session: &mut Session, wire: &[u8]) -> Result<Vec<u8>, ()> {
    let header: [u8; LENGTH_HEADER_LEN] =
        wire.get(..LENGTH_HEADER_LEN).ok_or(())?.try_into().map_err(|_| ())?;
    let len = session.receiver.decrypt_length(&header).map_err(|_| ())?;
    let mut body = wire.get(LENGTH_HEADER_LEN..).ok_or(())?.to_vec();
    if body.len() != len + 16 {
        return Err(());
    }
    Ok(session.receiver.decrypt_body(&mut body).map_err(|_| ())?.to_vec())
}

/// A client speaking tp2p over a socket, for attacks.
struct Peer {
    stream: TcpStream,
    session: Session,
}

impl Peer {
    fn connect(
        target: SocketAddr,
        key: &PublicKey,
        source: Ipv4Addr,
        rng: &mut Rng,
    ) -> std::io::Result<Self> {
        let mut stream = bind_connect(target, source)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let (initiator, act_one) =
            Initiator::new(rng.secret(), key, &NETWORK.prologue(), rng.secret());
        stream.write_all(&act_one)?;
        let mut two = [0u8; ACT_TWO_LEN];
        stream.read_exact(&mut two)?;
        let (three, session) =
            initiator.act_two(&two).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        stream.write_all(&three)?;
        Ok(Self { stream, session })
    }

    fn send_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        let mut out = Vec::new();
        self.session.sender.encrypt(chunk, &mut out);
        self.stream.write_all(&out)
    }

    fn send(&mut self, message: &Message) -> std::io::Result<()> {
        let mut chunks = Vec::new();
        chunk(message.type_byte(), &message.encode_body(), message.is_bulk(), |c| chunks.push(c));
        chunks.iter().try_for_each(|c| self.send_chunk(c))
    }

    /// Reads messages until `want` matches one, the peer closes, or the deadline passes.
    fn until(
        &mut self,
        deadline: Duration,
        want: impl Fn(&Message) -> bool,
    ) -> Result<Message, String> {
        let end = Instant::now() + deadline;
        let mut reassembler = Reassembler::default();
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("deadline".into());
            }
            self.stream.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
            let mut header = [0u8; LENGTH_HEADER_LEN];
            self.stream.read_exact(&mut header).map_err(read_error)?;
            let len =
                self.session.receiver.decrypt_length(&header).map_err(|e| format!("{e:?}"))?;
            let mut body = vec![0u8; len + 16];
            self.stream.read_exact(&mut body).map_err(read_error)?;
            let plain = self
                .session
                .receiver
                .decrypt_body(&mut body)
                .map_err(|e| format!("{e:?}"))?
                .to_vec();
            if let Some((type_byte, body)) =
                reassembler.push(&plain, |_| 64 << 20).map_err(|e| format!("{e:?}"))?
            {
                let message = Message::decode(type_byte, &body).map_err(|e| format!("{e:?}"))?;
                if want(&message) {
                    return Ok(message);
                }
            }
        }
    }

    /// Whether the node closes the connection within `deadline`.
    fn closed_within(&mut self, deadline: Duration) -> bool {
        self.until(deadline, |_| false)
            .is_err_and(|e| e.starts_with("closed") || e.contains("Disconnect"))
    }
}

/// A read timeout is the deadline passing, anything else the connection closing.
fn read_error(error: std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "deadline".into(),
        _ => format!("closed: {error}"),
    }
}

fn bind_connect(target: SocketAddr, source: Ipv4Addr) -> std::io::Result<TcpStream> {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
    socket.bind(&SocketAddr::V4(SocketAddrV4::new(source, 0)).into())?;
    socket.connect_timeout(&target.into(), Duration::from_secs(5))?;
    Ok(socket.into())
}

const fn status() -> Message {
    Message::Status(Status {
        version: 1,
        capabilities: 1,
        head_number: 0,
        head_hash: [0; 32],
        listen_port: 0,
    })
}

/// The process's resident memory in `KiB`.
fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn start_target(rng: &mut Rng) -> Result<(Handle, mpsc::Receiver<Event>, PublicKey), String> {
    let secret = rng.secret();
    let mut config = Config::new(NETWORK, secret, rng.next());
    config.capabilities = 1;
    config.requests = vec![RequestKind {
        kind: 1,
        capability: 1,
        max_request: 64,
        max_response: 1 << 20,
        per_peer_per_second: 10,
        timeout_ms: 2_000,
    }];
    let (handle, events) = start(config, Some("127.0.0.1:0".parse().expect("addr")), false)
        .map_err(|e| e.to_string())?;
    Ok((handle, events, PublicKey::from_secret_key(SECP256K1, &secret)))
}

/// One attack: what it did, whether the node defended (closed the attacker in time), and whether
/// an honest peer still got through afterwards.
struct Outcome {
    name: &'static str,
    defended: bool,
    honest_ok: bool,
    detail: String,
}

fn honest_check(target: SocketAddr, key: &PublicKey, rng: &mut Rng) -> bool {
    let Ok(mut peer) = Peer::connect(target, key, Ipv4Addr::new(127, 0, 0, 200), rng) else {
        return false;
    };
    if peer.send(&status()).is_err()
        || peer.until(Duration::from_secs(5), |m| matches!(m, Message::Status(_))).is_err()
    {
        return false;
    }
    let nonce = rng.next();
    peer.send(&Message::Ping(nonce)).is_ok()
        && peer.until(Duration::from_secs(5), |m| *m == Message::Pong(nonce)).is_ok()
}

/// Attacking peers against a live node.
fn live(seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let rss_start = rss_kib();
    let mut outcomes = Vec::new();
    type Attack = fn(SocketAddr, &PublicKey, &mut Rng) -> (bool, String);
    let attacks: [(&'static str, Attack); 9] = [
        ("slow-loris handshake (1 byte per second)", |target, _, _| {
            let Ok(mut stream) = bind_connect(target, Ipv4Addr::new(127, 0, 0, 10)) else {
                return (false, "connect".into());
            };
            let started = Instant::now();
            for _ in 0..ACT_ONE_LEN {
                if stream.write_all(&[0]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
                let mut probe = [0u8; 1];
                stream.set_nonblocking(true).ok();
                if matches!(stream.read(&mut probe), Ok(0)) {
                    break;
                }
                stream.set_nonblocking(false).ok();
            }
            let took = started.elapsed();
            (took <= HANDSHAKE_TIMEOUT + Duration::from_secs(3), format!("closed after {took:.1?}"))
        }),
        ("pending-handshake flood from one IP", |target, key, rng| {
            let streams: Vec<_> = (0..MAX_PENDING_HANDSHAKES_PER_IP * 8)
                .filter_map(|_| bind_connect(target, Ipv4Addr::new(127, 0, 0, 11)).ok())
                .collect();
            // Honest from another IP while the flood holds its sockets open.
            let honest = honest_check(target, key, rng);
            drop(streams);
            (honest, format!("honest peer during flood: {honest}"))
        }),
        ("garbage act one", |target, _, rng| {
            let Ok(mut stream) = bind_connect(target, Ipv4Addr::new(127, 0, 0, 12)) else {
                return (false, "connect".into());
            };
            let _ = stream.write_all(&rng.bytes(ACT_ONE_LEN));
            stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
            let mut buf = [0u8; ACT_TWO_LEN];
            let closed = !matches!(stream.read(&mut buf), Ok(n) if n > 0);
            (closed, format!("closed without act two: {closed}"))
        }),
        ("message before Status", |target, key, rng| {
            let Ok(mut peer) = Peer::connect(target, key, Ipv4Addr::new(127, 0, 0, 13), rng) else {
                return (false, "connect".into());
            };
            let _ = peer.send(&Message::Ping(1));
            (peer.closed_within(Duration::from_secs(3)), "ping first".into())
        }),
        ("declared length over the type limit", |target, key, rng| {
            let Ok(mut peer) = Peer::connect(target, key, Ipv4Addr::new(127, 0, 0, 14), rng) else {
                return (false, "connect".into());
            };
            let _ = peer.send(&status());
            let mut start = vec![FLAG_START, 0x01];
            start.extend_from_slice(&u32::MAX.to_be_bytes());
            let _ = peer.send_chunk(&start);
            (peer.closed_within(Duration::from_secs(3)), "ping declaring 4 GiB".into())
        }),
        ("chunk without a start", |target, key, rng| {
            let Ok(mut peer) = Peer::connect(target, key, Ipv4Addr::new(127, 0, 0, 15), rng) else {
                return (false, "connect".into());
            };
            let _ = peer.send(&status());
            let _ = peer.send_chunk(&[FLAG_END, 1, 2, 3]);
            (peer.closed_within(Duration::from_secs(3)), "end chunk first".into())
        }),
        ("partial message kept alive with pings", |target, key, rng| {
            let Ok(mut peer) = Peer::connect(target, key, Ipv4Addr::new(127, 0, 0, 16), rng) else {
                return (false, "connect".into());
            };
            let _ = peer.send(&status());
            let mut start = vec![FLAG_START | FLAG_BULK, 0x10];
            start.extend_from_slice(&40u32.to_be_bytes());
            start.extend_from_slice(&[0; 8]);
            let _ = peer.send_chunk(&start);
            // Keep the connection busy on the priority stream (each ping completes a message),
            // so neither the idle timeout nor a reset of the partial message's deadline can help.
            let started = Instant::now();
            let mut closed = false;
            while started.elapsed() < MESSAGE_TIMEOUT + Duration::from_secs(15) {
                if peer.send(&Message::Ping(started.elapsed().as_millis() as u64)).is_err()
                    || peer.closed_within(Duration::from_secs(5))
                {
                    closed = true;
                    break;
                }
            }
            (
                closed && started.elapsed() <= MESSAGE_TIMEOUT + Duration::from_secs(10),
                format!("closed after {:.1?}", started.elapsed()),
            )
        }),
        ("oversized IWant count", |target, key, rng| {
            let Ok(mut peer) = Peer::connect(target, key, Ipv4Addr::new(127, 0, 0, 17), rng) else {
                return (false, "connect".into());
            };
            let _ = peer.send(&status());
            let mut body = ((MAX_IDS + 1) as u32).to_be_bytes().to_vec();
            body.extend(vec![0u8; (MAX_IDS + 1) * 20]);
            let mut start = vec![FLAG_START | FLAG_END, 0x22];
            start.extend_from_slice(&(body.len() as u32).to_be_bytes());
            start.extend(body);
            let _ = peer.send_chunk(&start);
            (peer.closed_within(Duration::from_secs(3)), "257 ids".into())
        }),
        ("request flood over the rate", |target, key, rng| {
            let Ok(mut peer) = Peer::connect(target, key, Ipv4Addr::new(127, 0, 0, 18), rng) else {
                return (false, "connect".into());
            };
            let _ = peer.send(&status());
            for id in 0..2_000u32 {
                if peer.send(&Message::Request { id, kind: 1, body: vec![0; 8] }).is_err() {
                    break;
                }
            }
            // Over the rate the node answers Busy or disconnects; either way it must not queue
            // 2,000 requests for the application.
            let busy = peer
                .until(Duration::from_secs(5), |m| {
                    matches!(m, Message::Response { code: ResponseCode::Busy, .. })
                })
                .is_ok();
            (busy || peer.closed_within(Duration::from_secs(3)), format!("busy answers: {busy}"))
        }),
    ];
    for (name, attack) in attacks {
        let (handle, events, key) = start_target(&mut rng)?;
        let target = handle.local_addr().expect("listening");
        // Drain events so the node's event channel never backs up.
        let drain = std::thread::spawn(move || {
            let mut requests = 0u64;
            while let Ok(event) = events.recv() {
                requests += u64::from(matches!(event, Event::Request { .. }));
            }
            requests
        });
        let (defended, detail) = attack(target, &key, &mut rng);
        let honest_ok = honest_check(target, &key, &mut rng);
        handle.shutdown();
        let requests = drain.join().unwrap_or(0);
        outcomes.push(Outcome {
            name,
            defended,
            honest_ok,
            detail: format!("{detail}; {requests} requests reached the app"),
        });
    }
    let rss_end = rss_kib();
    let mut failed = 0;
    for o in &outcomes {
        let ok = o.defended && o.honest_ok;
        failed += usize::from(!ok);
        println!(
            "tp2p live: {:<44} {} (honest peer after: {}) {}",
            o.name,
            if o.defended { "defended" } else { "NOT DEFENDED" },
            o.honest_ok,
            o.detail
        );
    }
    println!("tp2p live: resident memory {rss_start} KiB -> {rss_end} KiB");
    if failed > 0 { Err(format!("{failed} attacks not defended")) } else { Ok(()) }
}
