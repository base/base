//! The live `RLPx` scenarios. Each scenario dials a node with a fresh key from its own loopback
//! address, does its exchange, and returns what it saw as normalized lines; then, if the
//! connection survived, a Ping probes that it is alive, and if it did not, a reconnect with the
//! same key shows whether the node now refuses the peer (a ban).

use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_primitives::{B256, hex};
use alloy_rlp::Decodable;

use super::peer::{Event, HelloSpec, Peer, Target, hello};
use crate::el::{
    rlp::{self, Item, Node, b, int, l},
    rng::Rng,
    wire::Seal,
};

/// Chain facts every scenario may use, read from the first target before the run.
#[derive(Debug, Clone)]
pub(crate) struct Ctx {
    /// Canonical block hashes by number, genesis first.
    pub(crate) hashes: Vec<B256>,
}

impl Ctx {
    const fn head(&self) -> u64 {
        self.hashes.len() as u64 - 1
    }

    fn hash(&self, n: usize) -> B256 {
        self.hashes[n.min(self.hashes.len() - 1)]
    }
}

/// Eth message ids are offset by the `p2p` range.
const ETH: u8 = 0x10;
/// Time to wait for a reaction.
const WAIT: Duration = Duration::from_millis(2500);

/// One scenario.
pub(crate) struct Scenario {
    /// Name.
    pub(crate) name: &'static str,
    /// Group (Hello, Status, headers, ...).
    pub(crate) group: &'static str,
    /// The exchange; returns what was seen.
    pub(crate) run: fn(&mut Run<'_>) -> Vec<String>,
}

/// One scenario run against one target.
pub(crate) struct Run<'a> {
    /// The node.
    pub(crate) target: &'a Target,
    /// Chain facts.
    pub(crate) ctx: &'a Ctx,
    /// Randomness (keys, nonces).
    pub(crate) rng: Rng,
    /// Our loopback source address.
    pub(crate) source: Ipv4Addr,
    /// The connection the scenario left open, for the liveness probe.
    pub(crate) peer: Option<Peer>,
    /// The key of the scenario's main connection, for the reconnect probe.
    pub(crate) key: Option<secp256k1::SecretKey>,
}

fn quantity(node: &Node) -> u64 {
    match &node.item {
        Item::Bytes(b) if b.len() <= 8 => b.iter().fold(0, |a, x| (a << 8) | *x as u64),
        _ => u64::MAX,
    }
}

fn items(payload: &[u8]) -> Option<Vec<Node>> {
    match rlp::parse(payload)?.0.item {
        Item::List(items) => Some(items),
        Item::Bytes(_) => None,
    }
}

fn summarize_numbers(numbers: &[u64]) -> String {
    if numbers.len() <= 8 {
        return format!("{numbers:?}");
    }
    format!(
        "[{}, {}, {} .. {}, {}]",
        numbers[0],
        numbers[1],
        numbers[2],
        numbers[numbers.len() - 2],
        numbers[numbers.len() - 1]
    )
}

fn describe_eth(id: u8, payload: &[u8]) -> String {
    let pair = items(payload);
    let request_id = pair.as_ref().and_then(|p| p.first()).map(quantity);
    let body_list = |pair: &Option<Vec<Node>>| -> Option<Vec<Node>> {
        match &pair.as_ref()?.get(1)?.item {
            Item::List(items) => Some(items.clone()),
            Item::Bytes(_) => None,
        }
    };
    match id {
        0x00 => "Status".into(),
        0x04 => {
            let numbers: Vec<u64> = body_list(&pair)
                .unwrap_or_default()
                .iter()
                .map(|h| {
                    Header::decode(&mut h.encode().as_slice()).map(|h| h.number).unwrap_or(u64::MAX)
                })
                .collect();
            format!(
                "BlockHeaders(id={:?}, n={}, numbers={})",
                request_id,
                numbers.len(),
                summarize_numbers(&numbers)
            )
        }
        0x06 | 0x10 => {
            let list = body_list(&pair).unwrap_or_default();
            let bytes: usize = list.iter().map(|n| n.encode().len()).sum();
            let name = if id == 0x06 { "BlockBodies" } else { "Receipts" };
            format!("{name}(id={request_id:?}, n={}, bytes={})", list.len(), bytes)
        }
        0x0a => format!(
            "PooledTransactions(id={request_id:?}, n={})",
            body_list(&pair).unwrap_or_default().len()
        ),
        0x11 => "BlockRangeUpdate".into(),
        _ => format!("eth(0x{id:02x}, {} bytes)", payload.len()),
    }
}

/// One normalized line per event; Pongs to our probe and unsolicited `BlockRangeUpdates` (sent
/// on a timer by some clients) are dropped.
fn describe(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Message(0x03, _) => Some("pong".into()),
            Event::Message(0x00, _) => Some("Hello".into()),
            Event::Message(id, p) if *id >= ETH => {
                let line = describe_eth(id - ETH, p);
                (line != "BlockRangeUpdate").then_some(line)
            }
            Event::Message(id, p) => Some(format!("p2p(0x{id:02x}, {} bytes)", p.len())),
            Event::Garbled(id, _) => Some(format!("garbled(0x{id:02x})")),
            Event::Disconnect(r) => Some(format!("disconnect({r})")),
            Event::Closed(_) => Some("closed".into()),
            Event::Timeout => Some("timeout".into()),
        })
        .collect()
}

impl Run<'_> {
    fn connect(&mut self) -> Result<Peer, Vec<String>> {
        let key = crate::el::wire::secret(&mut self.rng);
        self.key = Some(key);
        Peer::connect_with(self.target, self.source, &mut self.rng, key, Seal::default())
            .map_err(|e| vec![format!("connect failed: {e}")])
    }

    /// Hello with `spec`; returns the peer or what happened.
    fn hello(&mut self, spec: &HelloSpec) -> Result<Peer, Vec<String>> {
        let mut peer = self.connect()?;
        match hello(&mut peer, spec) {
            Ok(_) => Ok(peer),
            Err(events) => Err(describe(&events)),
        }
    }

    /// Hello (eth/`version`) and their Status; returns the peer and their Status tree.
    fn status_pending(&mut self, version: u64) -> Result<(Peer, Vec<Node>), Vec<String>> {
        let mut peer = self.hello(&HelloSpec::eth(version))?;
        match peer.recv(Duration::from_secs(5)) {
            Event::Message(id, payload) if id == ETH => items(&payload).map_or_else(
                || Err(vec!["their Status does not parse".into()]),
                |status| Ok((peer, status)),
            ),
            other => {
                let mut events = vec![other];
                events.extend(peer.observe(Duration::from_millis(500)));
                Err(describe(&events))
            }
        }
    }

    /// A full session: Hello, then their Status echoed back.
    fn session(&mut self, version: u64) -> Result<Peer, Vec<String>> {
        let (mut peer, status) = self.status_pending(version)?;
        peer.send(ETH, &l(status).encode()).map_err(|e| vec![e])?;
        Ok(peer)
    }

    /// Sends a message after the session is up and records the reaction.
    fn exchange(&mut self, version: u64, messages: &[(u8, Vec<u8>)]) -> Vec<String> {
        let mut peer = match self.session(version) {
            Ok(peer) => peer,
            Err(lines) => return lines,
        };
        for (id, payload) in messages {
            if let Err(e) = peer.send(*id, payload) {
                return vec![format!("send failed: {e}")];
            }
        }
        let lines = describe(&peer.observe(WAIT));
        self.peer = Some(peer);
        lines
    }

    /// Sends a request and waits for the matching response (so slow nodes are not cut off).
    fn request(&mut self, version: u64, id: u8, payload: Vec<u8>) -> Vec<String> {
        self.exchange(version, &[(id, payload)])
    }
}

fn get_headers(start: Node, limit: u64, skip: u64, reverse: bool) -> Vec<u8> {
    l(vec![int(7), l(vec![start, int(limit), int(skip), int(u64::from(reverse))])]).encode()
}

fn hashes_request(hashes: &[B256]) -> Vec<u8> {
    l(vec![int(9), l(hashes.iter().map(|h| b(h.to_vec())).collect())]).encode()
}

fn status_with(status: &[Node], index: usize, value: Node) -> Vec<u8> {
    let mut status = status.to_vec();
    status[index] = value;
    l(status).encode()
}

/// Runs `run` with the Status the node sent, changed by `edit`, sent back as ours.
fn status_variant(r: &mut Run<'_>, version: u64, edit: impl Fn(&[Node]) -> Vec<u8>) -> Vec<String> {
    let (mut peer, status) = match r.status_pending(version) {
        Ok(x) => x,
        Err(lines) => return lines,
    };
    let payload = edit(&status);
    if let Err(e) = peer.send(ETH, &payload) {
        return vec![e];
    }
    let lines = describe(&peer.observe(WAIT));
    r.peer = Some(peer);
    lines
}

/// eth/69 Status field indexes: version, network, genesis, fork id, earliest, latest, head.
const S69_NETWORK: usize = 1;
const S69_GENESIS: usize = 2;
const S69_FORK: usize = 3;
const S69_EARLIEST: usize = 4;
const S69_LATEST: usize = 5;
const S69_HEAD: usize = 6;

fn fork_hash(status: &[Node]) -> Vec<u8> {
    match &status[S69_FORK].item {
        Item::List(f) => match &f[0].item {
            Item::Bytes(h) => h.clone(),
            Item::List(_) => vec![0; 4],
        },
        Item::Bytes(_) => vec![0; 4],
    }
}

/// Every scenario.
pub(crate) fn all() -> Vec<Scenario> {
    macro_rules! s {
        ($group:literal, $name:literal, $f:expr) => {
            Scenario { group: $group, name: $name, run: $f }
        };
    }
    vec![
        // --- Hello and capability negotiation
        s!("hello", "eth69-baseline", |r| r.exchange(69, &[])),
        s!("hello", "eth68-only", |r| r.exchange(68, &[])),
        s!("hello", "eth66-67-only", |r| {
            let spec = HelloSpec { caps: vec![("eth", 66), ("eth", 67)], ..HelloSpec::eth(66) };
            hello_outcome(r, &spec)
        }),
        s!("hello", "eth70-71-only", |r| {
            let spec = HelloSpec { caps: vec![("eth", 70), ("eth", 71)], ..HelloSpec::eth(70) };
            hello_outcome(r, &spec)
        }),
        s!("hello", "snap-only", |r| hello_outcome(
            r,
            &HelloSpec { caps: vec![("snap", 1)], ..HelloSpec::eth(69) }
        )),
        s!("hello", "no-capabilities", |r| hello_outcome(
            r,
            &HelloSpec { caps: vec![], ..HelloSpec::eth(69) }
        )),
        s!("hello", "p2p-v4", |r| hello_outcome(
            r,
            &HelloSpec { version: 4, ..HelloSpec::eth(69) }
        )),
        s!("hello", "p2p-v6", |r| hello_outcome(
            r,
            &HelloSpec { version: 6, ..HelloSpec::eth(69) }
        )),
        s!("hello", "eip8-extra-fields", |r| {
            hello_outcome(
                r,
                &HelloSpec { extra: vec![b(b"future".to_vec()), int(7)], ..HelloSpec::eth(69) },
            )
        }),
        s!("hello", "id-differs-from-handshake", |r| {
            let id = r.rng.bytes(64);
            hello_outcome(r, &HelloSpec { id: Some(id), ..HelloSpec::eth(69) })
        }),
        s!("hello", "client-id-3000-bytes", |r| hello_outcome(
            r,
            &HelloSpec { client: vec![b'a'; 3000], ..HelloSpec::eth(69) }
        )),
        s!("hello", "first-frame-1MiB", |r| hello_outcome(
            r,
            &HelloSpec { client: vec![b'a'; 1 << 20], ..HelloSpec::eth(69) }
        )),
        s!("hello", "hello-snappy-compressed", |r| {
            let mut peer = match r.connect() {
                Ok(p) => p,
                Err(l) => return l,
            };
            let payload = HelloSpec::eth(69).encode(&peer);
            let data = snap::raw::Encoder::new().compress_vec(&payload).unwrap();
            let _ = peer.send_data(0x00, &data);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        s!("hello", "status-before-hello", |r| {
            let mut peer = match r.connect() {
                Ok(p) => p,
                Err(l) => return l,
            };
            let _ = peer.send_data(ETH, &[0xc0]);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        s!("hello", "second-hello", |r| {
            let mut peer = match r.session(69) {
                Ok(p) => p,
                Err(l) => return l,
            };
            let payload = HelloSpec::eth(69).encode(&peer);
            let _ = peer.send(0x00, &payload);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        s!("hello", "id-0x00-non-canonical", |r| {
            // Hello's id 0 as the raw byte 0x00 instead of RLP 0x80.
            let mut peer = match r.connect() {
                Ok(p) => p,
                Err(l) => return l,
            };
            let mut frame = vec![0x00];
            frame.extend(HelloSpec::eth(69).encode(&peer));
            let _ = peer.send_frame(&frame);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        // --- Status and fork id (eth/69 session)
        s!("status", "other-genesis", |r| status_variant(r, 69, |s| status_with(
            s,
            S69_GENESIS,
            b(vec![0x11; 32])
        ))),
        s!("status", "network-id-mismatch", |r| status_variant(r, 69, |s| status_with(
            s,
            S69_NETWORK,
            int(8453)
        ))),
        s!("status", "fork-hash-unknown", |r| {
            status_variant(r, 69, |s| {
                status_with(s, S69_FORK, l(vec![b(vec![0xde, 0xad, 0xbe, 0xef]), int(0)]))
            })
        }),
        s!("status", "fork-next-in-past", |r| {
            status_variant(r, 69, |s| {
                status_with(s, S69_FORK, l(vec![b(fork_hash(s)), int(1_600_000_000)]))
            })
        }),
        s!("status", "fork-next-in-future", |r| {
            status_variant(r, 69, |s| {
                status_with(s, S69_FORK, l(vec![b(fork_hash(s)), int(4_000_000_000)]))
            })
        }),
        s!("status", "version-68-in-eth69", |r| status_variant(r, 69, |s| status_with(
            s,
            0,
            int(68)
        ))),
        s!("status", "earliest-above-latest", |r| {
            status_variant(r, 69, |s| {
                let mut s = s.to_vec();
                s[S69_EARLIEST] = int(100);
                s[S69_LATEST] = int(5);
                l(s).encode()
            })
        }),
        s!("status", "zero-head-hash", |r| status_variant(r, 69, |s| status_with(
            s,
            S69_HEAD,
            b(vec![0; 32])
        ))),
        s!("status", "extra-field", |r| {
            status_variant(r, 69, |s| {
                let mut s = s.to_vec();
                s.push(int(1));
                l(s).encode()
            })
        }),
        s!("status", "undecodable", |r| status_variant(r, 69, |_| vec![0xc1, 0x80])),
        s!("status", "status-twice", |r| {
            let (mut peer, status) = match r.status_pending(69) {
                Ok(x) => x,
                Err(l) => return l,
            };
            let _ = peer.send(ETH, &l(status.clone()).encode());
            let _ = peer.send(ETH, &l(status).encode());
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        s!("status", "request-before-status", |r| {
            let (mut peer, _) = match r.status_pending(69) {
                Ok(x) => x,
                Err(l) => return l,
            };
            let _ = peer.send(ETH + 0x03, &get_headers(int(0), 1, 0, false));
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        s!("status", "ping-before-status", |r| {
            let (mut peer, status) = match r.status_pending(69) {
                Ok(x) => x,
                Err(l) => return l,
            };
            let _ = peer.send(0x02, &[0xc0]);
            let mut events = peer.observe(Duration::from_millis(800));
            let _ = peer.send(ETH, &l(status).encode());
            events.extend(peer.observe(Duration::from_millis(1500)));
            r.peer = Some(peer);
            describe(&events)
        }),
        s!("status", "no-status-timeout", |r| {
            let (mut peer, _) = match r.status_pending(69) {
                Ok(x) => x,
                Err(l) => return l,
            };
            let lines = describe(&peer.observe(Duration::from_secs(15)));
            r.peer = Some(peer);
            lines
        }),
        s!("status", "eth68-td-above-160-bits", |r| {
            // eth/68 Status: version, network, td, head, genesis, fork id.
            let (mut peer, status) = match r.status_pending(68) {
                Ok(x) => x,
                Err(l) => return l,
            };
            let _ = peer.send(ETH, &status_with(&status, 2, b(vec![0xff; 21])));
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        // --- GetBlockHeaders (eth/69)
        s!("headers", "by-number-0-count-5", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(int(0), 5, 0, false)
        )),
        s!("headers", "skip-1-count-3", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(int(1), 3, 1, false)
        )),
        s!("headers", "reverse-from-head-10", |r| {
            let head = r.ctx.head();
            r.request(69, ETH + 0x03, get_headers(int(head), 10, 0, true))
        }),
        s!("headers", "reverse-skip-past-genesis", |r| {
            let head = r.ctx.head();
            r.request(69, ETH + 0x03, get_headers(int(head), 10, 7, true))
        }),
        s!("headers", "count-0", |r| r.request(69, ETH + 0x03, get_headers(int(0), 0, 0, false))),
        s!("headers", "count-2000", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(int(0), 2000, 0, false)
        )),
        s!("headers", "count-u64-max", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(int(0), u64::MAX, 0, false)
        )),
        s!("headers", "unknown-number", |r| {
            let n = r.ctx.head() + 100;
            r.request(69, ETH + 0x03, get_headers(int(n), 5, 0, false))
        }),
        s!("headers", "unknown-hash", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(b(vec![0x42; 32]), 5, 0, false)
        )),
        s!("headers", "by-hash-block-2-reverse", |r| {
            let h = r.ctx.hash(2);
            r.request(69, ETH + 0x03, get_headers(b(h.to_vec()), 5, 0, true))
        }),
        s!("headers", "by-hash-forward-skip-2", |r| {
            let h = r.ctx.hash(1);
            r.request(69, ETH + 0x03, get_headers(b(h.to_vec()), 4, 2, false))
        }),
        s!("headers", "skip-u32-max-plus-1", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(int(0), 3, u32::MAX as u64 + 1, false)
        )),
        s!("headers", "skip-u32-max", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(int(0), 3, u32::MAX as u64, false)
        )),
        s!("headers", "start-plus-skip-overflow", |r| {
            r.request(69, ETH + 0x03, get_headers(int(1), 3, u64::MAX - 1, false))
        }),
        s!("headers", "start-u64-max", |r| r.request(
            69,
            ETH + 0x03,
            get_headers(int(u64::MAX), 3, 0, false)
        )),
        s!("headers", "request-extra-element", |r| {
            let payload = l(vec![int(7), l(vec![int(0), int(2), int(0), int(0), int(9)])]).encode();
            r.request(69, ETH + 0x03, payload)
        }),
        s!("headers", "envelope-extra-element", |r| {
            let payload = l(vec![int(7), l(vec![int(0), int(2), int(0), int(0)]), int(1)]).encode();
            r.request(69, ETH + 0x03, payload)
        }),
        s!("headers", "envelope-as-string", |r| {
            let inner =
                [int(7).encode(), l(vec![int(0), int(2), int(0), int(0)]).encode()].concat();
            r.request(69, ETH + 0x03, b(inner).encode())
        }),
        s!("headers", "request-id-u64-max", |r| {
            let payload = l(vec![int(u64::MAX), l(vec![int(0), int(1), int(0), int(0)])]).encode();
            r.request(69, ETH + 0x03, payload)
        }),
        s!("headers", "request-id-9-bytes", |r| {
            let payload = l(vec![b(vec![1; 9]), l(vec![int(0), int(1), int(0), int(0)])]).encode();
            r.request(69, ETH + 0x03, payload)
        }),
        s!("headers", "eth68-by-number", |r| r.request(
            68,
            ETH + 0x03,
            get_headers(int(0), 3, 0, false)
        )),
        // --- GetBlockBodies and GetReceipts
        s!("bodies", "known-1-3", |r| {
            let hashes = [r.ctx.hash(1), r.ctx.hash(2), r.ctx.hash(3)];
            r.request(69, ETH + 0x05, hashes_request(&hashes))
        }),
        s!("bodies", "unknown", |r| r.request(
            69,
            ETH + 0x05,
            hashes_request(&[B256::repeat_byte(0x42)])
        )),
        s!("bodies", "known-unknown-known", |r| {
            let hashes = [r.ctx.hash(1), B256::repeat_byte(0x42), r.ctx.hash(2)];
            r.request(69, ETH + 0x05, hashes_request(&hashes))
        }),
        s!("bodies", "duplicates", |r| {
            let hashes = vec![r.ctx.hash(1); 5];
            r.request(69, ETH + 0x05, hashes_request(&hashes))
        }),
        s!("bodies", "1100-duplicates", |r| {
            let hashes = vec![r.ctx.hash(1); 1100];
            r.request(69, ETH + 0x05, hashes_request(&hashes))
        }),
        s!("bodies", "big-blocks-soft-limit", |r| {
            // The first blocks carry 128 KiB deposits: 2 MiB is reached after ~16 of them.
            let hashes: Vec<B256> = (1..=r.ctx.head() as usize).map(|n| r.ctx.hash(n)).collect();
            r.request(69, ETH + 0x05, hashes_request(&hashes))
        }),
        s!("bodies", "big-block-repeated-1024", |r| {
            let hashes = vec![r.ctx.hash(1); 1024];
            r.request(69, ETH + 0x05, hashes_request(&hashes))
        }),
        s!("bodies", "genesis", |r| {
            let hashes = [r.ctx.hash(0)];
            r.request(69, ETH + 0x05, hashes_request(&hashes))
        }),
        s!("receipts", "known-1-3-eth69", |r| {
            let hashes = [r.ctx.hash(1), r.ctx.hash(2), r.ctx.hash(3)];
            r.request(69, ETH + 0x0f, hashes_request(&hashes))
        }),
        s!("receipts", "known-1-3-eth68", |r| {
            let hashes = [r.ctx.hash(1), r.ctx.hash(2), r.ctx.hash(3)];
            r.request(68, ETH + 0x0f, hashes_request(&hashes))
        }),
        s!("receipts", "unknown", |r| r.request(
            69,
            ETH + 0x0f,
            hashes_request(&[B256::repeat_byte(0x42)])
        )),
        s!("receipts", "known-unknown-known", |r| {
            let hashes = [r.ctx.hash(1), B256::repeat_byte(0x42), r.ctx.hash(2)];
            r.request(69, ETH + 0x0f, hashes_request(&hashes))
        }),
        s!("receipts", "1100-duplicates", |r| {
            let hashes = vec![r.ctx.hash(1); 1100];
            r.request(69, ETH + 0x0f, hashes_request(&hashes))
        }),
        s!("receipts", "genesis", |r| {
            let hashes = [r.ctx.hash(0)];
            r.request(69, ETH + 0x0f, hashes_request(&hashes))
        }),
        s!("pooled", "get-pooled-transactions", |r| r.request(
            69,
            ETH + 0x09,
            hashes_request(&[B256::repeat_byte(1)])
        )),
        s!("pooled", "get-pooled-garbage-body", |r| r.request(
            69,
            ETH + 0x09,
            l(vec![int(3), int(5)]).encode()
        )),
        // --- Message size limits and Snappy
        s!("size", "eth-message-11MiB", |r| {
            // A Transactions message of 11 MiB (compresses to a few hundred KiB).
            let payload = l(vec![b(vec![0; 11 << 20])]).encode();
            r.exchange(69, &[(ETH + 0x02, payload)])
        }),
        s!("size", "eth-message-9MiB", |r| {
            let payload = l(vec![b(vec![0; 9 << 20])]).encode();
            r.exchange(69, &[(ETH + 0x02, payload)])
        }),
        s!("size", "snappy-declared-16MiB-plus-1", |r| raw_data(
            r,
            ETH + 0x02,
            snappy_declared((16 << 20) + 1, &[0xc0])
        )),
        s!("size", "snappy-declared-16MiB-exact-short", |r| raw_data(
            r,
            ETH + 0x02,
            snappy_declared(16 << 20, &[0xc0])
        )),
        s!("size", "snappy-declared-longer", |r| raw_data(
            r,
            ETH + 0x02,
            snappy_declared(3, &[0xc0])
        )),
        s!("size", "snappy-declared-shorter", |r| raw_data(
            r,
            ETH + 0x02,
            snappy_declared(1, &[0xc2, 0x80, 0x80])
        )),
        s!("size", "uncompressed-after-hello", |r| raw_data(
            r,
            ETH + 0x03,
            get_headers(int(0), 1, 0, false)
        )),
        s!("size", "empty-frame-payload-size-0", |r| {
            let mut peer = match r.session(69) {
                Ok(p) => p,
                Err(l) => return l,
            };
            let mut bytes = peer.writer.header(crate::el::wire::header_plain(0));
            bytes.extend(peer.writer.body(&[]));
            let _ = peer.write_raw(&bytes);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        s!("size", "frame-declares-more-than-sent", |r| {
            let mut peer = match r.session(69) {
                Ok(p) => p,
                Err(l) => return l,
            };
            let mut bytes = peer.writer.header(crate::el::wire::header_plain(1000));
            bytes.extend(peer.writer.body(&[0x13, 0x00]));
            let _ = peer.write_raw(&bytes);
            let lines = describe(&peer.observe(Duration::from_secs(6)));
            r.peer = Some(peer);
            lines
        }),
        s!("size", "bad-header-mac", |r| {
            let mut peer = match r.session(69) {
                Ok(p) => p,
                Err(l) => return l,
            };
            let mut bytes = peer.writer.frame(&[0x02, 0x01, 0x00, 0xc0]);
            bytes[20] ^= 1;
            let _ = peer.write_raw(&bytes);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        s!("size", "auth-padding-2000", |r| {
            let key = crate::el::wire::secret(&mut r.rng);
            r.key = Some(key);
            match Peer::connect_with(
                r.target,
                r.source,
                &mut r.rng,
                key,
                Seal { padding: 2000, ..Seal::default() },
            ) {
                Ok(mut peer) => {
                    let out = match hello(&mut peer, &HelloSpec::eth(69)) {
                        Ok(_) => vec!["handshake ok".to_string(), "Hello".to_string()],
                        Err(e) => describe(&e),
                    };
                    r.peer = Some(peer);
                    out
                }
                Err(e) => vec![format!("handshake failed: {}", e.split(':').next().unwrap_or(""))],
            }
        }),
        // --- Message ids
        s!("ids", "reserved-p2p-0x05", |r| r.exchange(69, &[(0x05, vec![0xc0])])),
        s!("ids", "eth-unassigned-0x0b", |r| r
            .exchange(69, &[(ETH + 0x0b, l(vec![int(1), l(vec![])]).encode())])),
        s!("ids", "eth-get-node-data-0x0d", |r| r
            .exchange(69, &[(ETH + 0x0d, l(vec![int(1), l(vec![])]).encode())])),
        s!("ids", "eth-past-range-0x12", |r| r.exchange(69, &[(ETH + 0x12, vec![0xc0])])),
        s!("ids", "eth68-block-range-update", |r| {
            let payload = l(vec![int(0), int(1), b(vec![1; 32])]).encode();
            r.exchange(68, &[(ETH + 0x11, payload)])
        }),
        s!("ids", "id-0x7f", |r| r.exchange(69, &[(0x7f, vec![0xc0])])),
        s!("ids", "status-after-handshake", |r| {
            let mut peer = match r.session(69) {
                Ok(p) => p,
                Err(l) => return l,
            };
            let _ = peer.send(ETH, &[0xc0]);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
        // --- Responses nobody asked for, and broadcasts
        s!("unsolicited", "block-headers", |r| r
            .exchange(69, &[(ETH + 0x04, l(vec![int(77), l(vec![])]).encode())])),
        s!("unsolicited", "block-bodies", |r| r
            .exchange(69, &[(ETH + 0x06, l(vec![int(77), l(vec![])]).encode())])),
        s!("unsolicited", "receipts", |r| r
            .exchange(69, &[(ETH + 0x10, l(vec![int(77), l(vec![])]).encode())])),
        s!("unsolicited", "pooled-transactions", |r| r
            .exchange(69, &[(ETH + 0x0a, l(vec![int(77), l(vec![])]).encode())])),
        s!("unsolicited", "block-headers-garbage", |r| r
            .exchange(69, &[(ETH + 0x04, l(vec![int(77), b(vec![1, 2])]).encode())])),
        s!("unsolicited", "response-without-id", |r| r
            .exchange(69, &[(ETH + 0x04, l(vec![]).encode())])),
        s!("broadcast", "transactions-garbage", |r| r
            .exchange(69, &[(ETH + 0x02, l(vec![b(vec![0x7f, 1, 2])]).encode())])),
        s!("broadcast", "transactions-empty", |r| r
            .exchange(69, &[(ETH + 0x02, l(vec![]).encode())])),
        s!("broadcast", "pooled-hashes-length-mismatch", |r| {
            let payload =
                l(vec![b(vec![2, 2]), l(vec![int(100)]), l(vec![b(vec![1; 32]), b(vec![2; 32])])])
                    .encode();
            r.exchange(69, &[(ETH + 0x08, payload)])
        }),
        s!("broadcast", "pooled-hashes-valid", |r| {
            let payload = l(vec![b(vec![2]), l(vec![int(100)]), l(vec![b(vec![1; 32])])]).encode();
            r.exchange(69, &[(ETH + 0x08, payload)])
        }),
        s!("broadcast", "new-block-hashes", |r| r
            .exchange(69, &[(ETH + 0x01, l(vec![l(vec![b(vec![1; 32]), int(5)])]).encode())])),
        s!("broadcast", "new-block-garbage", |r| r
            .exchange(69, &[(ETH + 0x07, l(vec![int(1)]).encode())])),
        s!("broadcast", "block-range-earliest-above-latest", |r| {
            r.exchange(69, &[(ETH + 0x11, l(vec![int(9), int(3), b(vec![1; 32])]).encode())])
        }),
        s!("broadcast", "block-range-zero-hash", |r| r
            .exchange(69, &[(ETH + 0x11, l(vec![int(0), int(3), b(vec![0; 32])]).encode())])),
        s!("broadcast", "block-range-extra-field", |r| {
            r.exchange(
                69,
                &[(ETH + 0x11, l(vec![int(0), int(3), b(vec![1; 32]), int(1)]).encode())],
            )
        }),
        // --- p2p
        s!("p2p", "ping", |r| r.exchange(69, &[(0x02, vec![0xc0])])),
        s!("p2p", "ping-payload-c180", |r| r.exchange(69, &[(0x02, vec![0xc1, 0x80])])),
        s!("p2p", "ping-flood-50", |r| {
            let pings: Vec<(u8, Vec<u8>)> = (0..50).map(|_| (0x02, vec![0xc0])).collect();
            let lines = r.exchange(69, &pings);
            let pongs = lines.iter().filter(|l| *l == "pong").count();
            let mut out: Vec<String> = lines.into_iter().filter(|l| l != "pong").collect();
            out.insert(0, format!("{pongs} pongs"));
            out
        }),
        s!("p2p", "disconnect-uncompressed", |r| {
            let mut peer = match r.session(69) {
                Ok(p) => p,
                Err(l) => return l,
            };
            let _ = peer.send_data(0x01, &[0xc1, 0x08]);
            let lines = describe(&peer.observe(WAIT));
            r.peer = Some(peer);
            lines
        }),
    ]
}

/// Hello with `spec`, then whatever follows (Status, Disconnect, ...).
fn hello_outcome(r: &mut Run<'_>, spec: &HelloSpec) -> Vec<String> {
    match r.hello(spec) {
        Ok(mut peer) => {
            let mut lines = vec!["Hello".to_string()];
            lines.extend(describe(&peer.observe(WAIT)));
            r.peer = Some(peer);
            lines
        }
        Err(lines) => lines,
    }
}

/// A Snappy block that declares `len` but carries `data` as one literal.
fn snappy_declared(len: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut v = len;
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    out.push(((data.len() - 1) as u8) << 2);
    out.extend_from_slice(data);
    out
}

/// Sends message `id` with `data` as is (no compression) after the session is up.
fn raw_data(r: &mut Run<'_>, id: u8, data: Vec<u8>) -> Vec<String> {
    let mut peer = match r.session(69) {
        Ok(p) => p,
        Err(l) => return l,
    };
    let _ = peer.send_data(id, &data);
    let lines = describe(&peer.observe(WAIT));
    r.peer = Some(peer);
    lines
}

/// Runs one scenario against one target, with the liveness and reconnect probes.
pub(crate) fn run_one(
    scenario: &Scenario,
    target: &Target,
    ctx: &Ctx,
    seed: u64,
    source: Ipv4Addr,
) -> String {
    let mut run = Run { target, ctx, rng: Rng::new(seed), source, peer: None, key: None };
    let started = Instant::now();
    let mut lines = (scenario.run)(&mut run);
    let closed = lines.iter().any(|l| {
        l == "closed"
            || l.starts_with("disconnect")
            || l.starts_with("connect failed")
            || l.starts_with("handshake failed")
    });
    if let Some(mut peer) = run.peer.take()
        && !closed
    {
        let _ = peer.send(0x02, &[0xc0]);
        let probe = describe(&peer.observe(Duration::from_secs(2)));
        lines.push(if probe.iter().any(|l| l == "pong") {
            "alive".into()
        } else {
            format!("probe: {}", probe.join(" "))
        });
    }
    if closed && let Some(key) = run.key {
        // Same key, same address: does the node take the peer back?
        let mut rng = Rng::case(seed, 0xb4);
        let probe = match Peer::connect_with(target, source, &mut rng, key, Seal::default()) {
            Ok(mut peer) => match hello(&mut peer, &HelloSpec::eth(69)) {
                Ok(_) => match peer.recv(Duration::from_secs(3)) {
                    Event::Message(id, _) if id == ETH => "status".to_string(),
                    other => describe(&[other]).join(" "),
                },
                Err(events) => describe(&events).join(" "),
            },
            Err(e) => format!("refused ({})", e.split(':').next().unwrap_or("")),
        };
        lines.push(format!("reconnect: {probe}"));
    }
    let _ = started;
    lines.join(" ; ")
}

/// Reads the chain's hashes from `target` (`GetBlockHeaders` from genesis).
pub(crate) fn read_ctx(target: &Target) -> Result<Ctx, String> {
    let mut rng = Rng::new(1);
    let mut peer = Peer::connect(target, Ipv4Addr::new(127, 0, 0, 1), &mut rng)?;
    hello(&mut peer, &HelloSpec::eth(69)).map_err(|e| format!("hello: {e:?}"))?;
    let status = match peer.recv(Duration::from_secs(5)) {
        Event::Message(id, p) if id == ETH => p,
        other => return Err(format!("no status: {other:?}")),
    };
    peer.send(ETH, &status)?;
    peer.send(ETH + 0x03, &get_headers(int(0), 1024, 0, false))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Event::Message(id, payload) = peer.recv(Duration::from_secs(5))
            && id == ETH + 0x04
        {
            let pair = items(&payload).ok_or("headers")?;
            let Item::List(headers) = &pair[1].item else { return Err("headers".into()) };
            let hashes = headers.iter().map(|h| alloy_primitives::keccak256(h.encode())).collect();
            return Ok(Ctx { hashes });
        }
    }
    Err("no headers".into())
}

/// Prints `hashes` for the report.
pub(crate) fn describe_ctx(ctx: &Ctx) -> String {
    format!(
        "{} blocks, head {} {}",
        ctx.hashes.len(),
        ctx.head(),
        hex::encode(ctx.hashes.last().unwrap())
    )
}
