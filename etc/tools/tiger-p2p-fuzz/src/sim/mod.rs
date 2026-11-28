//! Deterministic multi-node simulation of tp2p (`sim`).
//!
//! Many `base_tiger_p2p::Node` state machines run against a simulated network in one thread.
//! The network has seeded per-link latency, a bandwidth delay per byte, FIFO order per
//! connection (TCP), partitions that reset the connections crossing them, and malicious nodes.
//! Everything, including every node's choices, follows from `--seed`, so a failing seed replays
//! exactly. The node code is the production code; only sockets and the clock are simulated.
//!
//! The application on each honest node models consensus gossip validation the way
//! base-consensus's `BlockHandler` does: a message is `height || variant || valid flag ||
//! padding`; invalid ones are rejected, at most 5 distinct messages per height are accepted, and
//! a repeat is ignored. Requests model witness fetches: the serving node answers with the
//! requested height, and a bad responder answers with garbage the requester reports.
//!
//! Checked invariants (each failure is printed and makes the exit status nonzero):
//! - safety: an honest node sends a gossip message in full only after accepting or publishing it;
//! - liveness: outside partitions (plus a settling time) every valid published message reaches
//!   every honest node within `--deadline-ms`;
//! - eclipse: a victim whose peer-exchange answers come mostly from attackers keeps at least one
//!   honest outbound peer and keeps receiving messages;
//! - requests: an honest requester gets a correct answer for each block despite bad responders;
//! - bounds: no node panics, and per-connection queues stay below a fixed cap.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
};

use base_tiger_p2p::{
    Config, ConnId, Direction, Event, Message, MessageId, Network, Node, Output, Penalty,
    RequestError, RequestKind, ResponseCode, Rng, Target, Validation, message_id,
};
use secp256k1_tiger::{PublicKey, SECP256K1, SecretKey};

const NET: Network = Network { chain_id: 8453, genesis: [7; 32], layer: Network::CONSENSUS };
const TOPIC: u8 = 3;
const WITNESS_KIND: u16 = 1;
const WITNESS_CAP: u32 = 1;
const MAX_PER_HEIGHT: usize = 5;
const TICK_MS: u64 = 100;
const BYTES_PER_MS: u64 = 12_500; // 100 Mbit/s per link
const MAX_QUEUED_PER_CONN: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Honest,
    /// Answers every peer exchange with attacker records only, dials the victim, relays nothing.
    Eclipse,
    /// Sends invalid gossip, bogus announcements and want floods as fast as allowed and beyond.
    Flood,
    /// Accepts connections but never sends Status.
    Silent,
    /// Answers requests with garbage.
    BadResponder,
}

#[derive(Debug, Default)]
struct App {
    accepted: HashSet<MessageId>,
    published: HashSet<MessageId>,
    per_height: HashMap<u64, HashSet<MessageId>>,
    accepted_at: HashMap<MessageId, u64>,
    witnesses_ok: u64,
    witnesses_bad: u64,
    pending_witness: HashMap<u64, (u64, u64, Vec<[u8; 33]>)>,
    witness_latency: Vec<u64>,
}

struct SimNode {
    node: Node,
    key: PublicKey,
    addr: SocketAddr,
    behavior: Behavior,
    app: App,
    side: u8,
    next_conn: ConnId,
}

struct Delivery {
    to: usize,
    conn: ConnId,
    kind: DeliveryKind,
}

enum DeliveryKind {
    Message(Message),
    Closed,
}

/// Options for one run.
#[derive(Debug, Clone)]
struct Options {
    scenario: String,
    seed: u64,
    nodes: usize,
    attackers: usize,
    attacker_groups: usize,
    duration_ms: u64,
    deadline_ms: u64,
    block_ms: u64,
    payload: usize,
    partition: Option<(u64, u64)>,
    fanout: usize,
    fanout_large: Option<usize>,
    verbose: bool,
}

struct Sim {
    opts: Options,
    now: u64,
    nodes: Vec<SimNode>,
    by_addr: HashMap<SocketAddr, usize>,
    links: HashMap<(usize, ConnId), (usize, ConnId)>,
    queue: BTreeMap<(u64, u64), Delivery>,
    fifo: HashMap<(usize, ConnId), u64>,
    queued: HashMap<(usize, ConnId), usize>,
    seq: u64,
    rng: Rng,
    published: Vec<(u64, MessageId, u64)>,
    violations: Vec<String>,
    bytes: u64,
    partitioned: bool,
    victim: Option<usize>,
    uplink: Vec<u64>,
}

fn secret(i: usize, seed: u64) -> SecretKey {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_be_bytes());
    bytes[8..16].copy_from_slice(&(i as u64 + 1).to_be_bytes());
    bytes[31] = 1;
    SecretKey::from_byte_array(bytes).expect("valid key")
}

fn payload(height: u64, variant: u8, valid: bool, size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size.max(10)];
    data[..8].copy_from_slice(&height.to_be_bytes());
    data[8] = variant;
    data[9] = u8::from(valid);
    data
}

fn encoded_len(message: &Message) -> u64 {
    // Body, type byte, chunk header and Noise framing (one chunk per 64 KiB).
    let body = message.encode_body().len() as u64;
    body + 6 + (body / 65_000 + 1) * 34
}

impl Sim {
    fn new(opts: Options) -> Self {
        let mut rng = Rng::new(opts.seed);
        let mut nodes = Vec::new();
        let mut by_addr = HashMap::new();
        let total = opts.nodes + opts.attackers;
        // Honest nodes get distinct /16 groups; attackers share `attacker_groups` groups.
        for i in 0..total {
            let honest = i < opts.nodes;
            let ip = if honest {
                [11 + (i / 200) as u8, (i % 200) as u8, 1, 1]
            } else {
                let g = (i - opts.nodes) % opts.attacker_groups.max(1);
                [200, g as u8, ((i >> 8) & 0xff) as u8, (i & 0xff) as u8]
            };
            let addr = SocketAddr::from((ip, 30400));
            let behavior = if honest {
                Behavior::Honest
            } else {
                match opts.scenario.as_str() {
                    "eclipse" => Behavior::Eclipse,
                    "flood" => Behavior::Flood,
                    "silent" => Behavior::Silent,
                    "requests" => Behavior::BadResponder,
                    _ => [
                        Behavior::Flood,
                        Behavior::Silent,
                        Behavior::BadResponder,
                        Behavior::Eclipse,
                    ][(i - opts.nodes) % 4],
                }
            };
            let sk = secret(i, opts.seed);
            let mut config = Config::new(NET, sk, rng.next_u64());
            config.advertise = Some(addr);
            config.capabilities = WITNESS_CAP;
            config.requests.push(RequestKind {
                kind: WITNESS_KIND,
                capability: WITNESS_CAP,
                max_request: 8,
                max_response: 1 << 20,
                per_peer_per_second: 20,
                timeout_ms: 2_000,
            });
            config.max_gossip_len = 1 << 20;
            config.gossip_fanout = opts.fanout;
            if let Some(f) = opts.fanout_large {
                config.gossip_fanout_large = f;
            }
            by_addr.insert(addr, i);
            nodes.push(SimNode {
                key: PublicKey::from_secret_key(SECP256K1, &sk),
                node: Node::new(config, 1),
                addr,
                behavior,
                app: App::default(),
                side: (i % 2) as u8,
                next_conn: 1,
            });
        }
        // Bootnodes: every node knows honest node 0 and 1. In the eclipse scenario, node `nodes-1`
        // is the victim and also knows nothing else; attackers know the victim.
        let boots: Vec<(PublicKey, SocketAddr)> =
            (0..2.min(opts.nodes)).map(|i| (nodes[i].key, nodes[i].addr)).collect();
        let victim = (opts.scenario == "eclipse").then_some(opts.nodes - 1);
        for i in 0..total {
            let mut config = nodes[i].node.config().clone();
            config.bootnodes = boots.iter().filter(|(k, _)| *k != nodes[i].key).copied().collect();
            if nodes[i].behavior == Behavior::Eclipse
                && let Some(v) = victim
            {
                config.bootnodes.push((nodes[v].key, nodes[v].addr));
                config.max_outbound = 2;
            }
            nodes[i].node = Node::new(config, 1);
        }
        Self {
            opts,
            now: 1,
            nodes,
            by_addr,
            links: HashMap::new(),
            queue: BTreeMap::new(),
            fifo: HashMap::new(),
            queued: HashMap::new(),
            seq: 0,
            rng,
            published: Vec::new(),
            violations: Vec::new(),
            bytes: 0,
            partitioned: false,
            victim,
            uplink: vec![0; total],
        }
    }

    fn latency(&self, a: usize, b: usize) -> u64 {
        let (x, y) = (a.min(b) as u64, a.max(b) as u64);
        let h = (x.wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ y.wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
            ^ self.opts.seed)
            .wrapping_mul(0x2545_f491_4f6c_dd1d);
        5 + (h >> 33) % 95
    }

    fn crosses(&self, a: usize, b: usize) -> bool {
        self.partitioned && self.nodes[a].side != self.nodes[b].side
    }

    fn schedule(&mut self, from: usize, to: usize, conn: ConnId, kind: DeliveryKind, size: u64) {
        let key = (to, conn);
        // Each node's uplink sends one message at a time: a message leaves when the previous
        // ones have, then crosses the link.
        let departs = self.now.max(self.uplink[from]) + size / BYTES_PER_MS;
        self.uplink[from] = departs;
        let mut at = departs + self.latency(from, to);
        if let Some(last) = self.fifo.get(&key) {
            at = at.max(*last);
        }
        self.fifo.insert(key, at);
        let q = self.queued.entry(key).or_default();
        *q += 1;
        if *q > MAX_QUEUED_PER_CONN {
            self.violations
                .push(format!("queue to node {to} conn {conn} above {MAX_QUEUED_PER_CONN}"));
        }
        self.seq += 1;
        self.queue.insert((at, self.seq), Delivery { to, conn, kind });
    }

    fn send(&mut self, from: usize, conn: ConnId, message: Message) {
        let Some(&(to, peer_conn)) = self.links.get(&(from, conn)) else { return };
        if self.nodes[from].behavior == Behavior::Honest
            && let Message::Gossip { topic, data } = &message
        {
            let id = message_id(*topic, data);
            let app = &self.nodes[from].app;
            if !app.accepted.contains(&id) && !app.published.contains(&id) {
                self.violations.push(format!(
                    "t={} node {from} forwarded {id:02x?} before accepting it",
                    self.now
                ));
            }
        }
        let size = encoded_len(&message);
        self.bytes += size;
        self.schedule(from, to, peer_conn, DeliveryKind::Message(message), size);
    }

    fn close_link(&mut self, from: usize, conn: ConnId) {
        if let Some((to, peer_conn)) = self.links.remove(&(from, conn)) {
            self.links.remove(&(to, peer_conn));
            self.schedule(from, to, peer_conn, DeliveryKind::Closed, 0);
        }
    }

    fn dial(&mut self, from: usize, key: PublicKey, addr: SocketAddr) {
        if self.opts.verbose && Some(from) == self.victim {
            let to = self.by_addr.get(&addr).copied();
            eprintln!("t={} victim dials {:?} -> {to:?}", self.now, addr);
        }
        let Some(&to) = self.by_addr.get(&addr) else {
            self.nodes[from].node.dial_failed(&key);
            return;
        };
        if self.nodes[to].key != key || self.crosses(from, to) || to == from {
            self.nodes[from].node.dial_failed(&key);
            return;
        }
        let (c_from, c_to) = (self.nodes[from].next_conn, self.nodes[to].next_conn);
        self.nodes[from].next_conn += 1;
        self.nodes[to].next_conn += 1;
        let (from_key, from_addr, now) = (self.nodes[from].key, self.nodes[from].addr, self.now);
        if self.nodes[to].behavior == Behavior::Silent {
            // Takes the connection and never answers; the dialer times it out.
            self.links.insert((from, c_from), (to, c_to));
            self.links.insert((to, c_to), (from, c_from));
            let _ = self.nodes[from].node.connected(c_from, key, addr, Direction::Outbound, now);
            return;
        }
        if self.nodes[to]
            .node
            .connected(c_to, from_key, from_addr, Direction::Inbound, now)
            .is_err()
        {
            self.nodes[from].node.dial_failed(&key);
            return;
        }
        if self.nodes[from].node.connected(c_from, key, addr, Direction::Outbound, now).is_err() {
            self.nodes[to].node.disconnected(c_to);
            return;
        }
        self.links.insert((from, c_from), (to, c_to));
        self.links.insert((to, c_to), (from, c_from));
    }

    /// Drains a node's outputs and runs its application.
    fn drain(&mut self, i: usize) {
        while let Some(output) = self.nodes[i].node.poll() {
            let behavior = self.nodes[i].behavior;
            match output {
                Output::Send { conn, message } => {
                    if behavior == Behavior::Silent {
                        continue;
                    }
                    self.send(i, conn, message);
                }
                Output::Close { conn, .. } => self.close_link(i, conn),
                Output::Dial { key, addr } => self.dial(i, key, addr),
                Output::Event(event) => self.app_event(i, event),
            }
        }
    }

    fn app_event(&mut self, i: usize, event: Event) {
        let now = self.now;
        let behavior = self.nodes[i].behavior;
        match event {
            Event::Gossip { id, data, .. } => {
                let verdict = if behavior != Behavior::Honest {
                    Validation::Ignore
                } else if data.len() < 10 || data[9] != 1 {
                    Validation::Reject
                } else {
                    let height = u64::from_be_bytes(data[..8].try_into().expect("8 bytes"));
                    let app = &mut self.nodes[i].app;
                    let at_height = app.per_height.entry(height).or_default();
                    if at_height.len() >= MAX_PER_HEIGHT {
                        Validation::Reject
                    } else if !at_height.insert(id) {
                        Validation::Ignore
                    } else {
                        app.accepted.insert(id);
                        app.accepted_at.insert(id, now);
                        Validation::Accept
                    }
                };
                self.nodes[i].node.validated(&id, verdict, now);
            }
            Event::Request { peer, id, kind, body } => {
                let answer = match behavior {
                    Behavior::BadResponder => vec![0xee; 64],
                    _ if i == 0 || behavior == Behavior::Honest => {
                        let mut w = body;
                        w.extend_from_slice(&[0xaa; 1000]);
                        w
                    }
                    _ => return,
                };
                self.nodes[i].node.respond(&peer, id, kind, ResponseCode::Ok, answer);
            }
            Event::Response { request, peer, result } => {
                let Some(height) = self.nodes[i]
                    .app
                    .pending_witness
                    .iter()
                    .find(|(_, v)| v.0 == request)
                    .map(|(h, _)| *h)
                else {
                    return;
                };
                match result {
                    Ok(body) if body.len() == 1008 && body[..8] == height.to_be_bytes() => {
                        let (_, started, _) =
                            self.nodes[i].app.pending_witness.remove(&height).expect("found");
                        self.nodes[i].app.witnesses_ok += 1;
                        self.nodes[i].app.witness_latency.push(now - started);
                    }
                    other => {
                        if other.is_ok() {
                            self.nodes[i].app.witnesses_bad += 1;
                            self.nodes[i].node.report(&peer, Penalty::BadResponse, now);
                        }
                        let entry =
                            self.nodes[i].app.pending_witness.get_mut(&height).expect("found");
                        if peer != [0; 33] {
                            entry.2.push(peer);
                        }
                        let except = entry.2.clone();
                        let retry = if matches!(other, Err(RequestError::NoPeer)) {
                            Target::Any
                        } else {
                            Target::AnyExcept(except)
                        };
                        let id = self.nodes[i].node.request(
                            WITNESS_KIND,
                            height.to_be_bytes().to_vec(),
                            retry,
                            now,
                        );
                        self.nodes[i].app.pending_witness.get_mut(&height).expect("found").0 = id;
                    }
                }
            }
            Event::PeerReady { .. } | Event::PeerGone { .. } | Event::Notify { .. } => {}
        }
    }

    fn deliver(&mut self, d: Delivery) {
        let key = (d.to, d.conn);
        if let Some(q) = self.queued.get_mut(&key) {
            *q = q.saturating_sub(1);
        }
        let now = self.now;
        match d.kind {
            DeliveryKind::Closed => {
                self.links.remove(&key);
                self.nodes[d.to].node.disconnected(d.conn);
            }
            DeliveryKind::Message(message) => {
                if !self.links.contains_key(&key) {
                    return;
                }
                let behavior = self.nodes[d.to].behavior;
                if behavior == Behavior::Eclipse && matches!(message, Message::GetPeers(_)) {
                    let records = self.attacker_records();
                    self.send(d.to, d.conn, Message::Peers(records));
                    return;
                }
                if behavior == Behavior::Silent {
                    return;
                }
                self.nodes[d.to].node.received(d.conn, message, now);
            }
        }
        self.drain(d.to);
    }

    fn attacker_records(&mut self) -> Vec<base_tiger_p2p::PeerRecord> {
        let attackers: Vec<usize> = (self.opts.nodes..self.nodes.len()).collect();
        let mut out = Vec::new();
        for _ in 0..32.min(attackers.len()) {
            let a = attackers[self.rng.below(attackers.len() as u64) as usize];
            let sk = secret(a, self.opts.seed);
            out.push(base_tiger_p2p::PeerRecord::signed(
                &sk,
                &NET.id(),
                self.nodes[a].addr,
                1,
                WITNESS_CAP,
            ));
        }
        out
    }

    fn attack(&mut self) {
        for i in self.opts.nodes..self.nodes.len() {
            if self.nodes[i].behavior != Behavior::Flood {
                continue;
            }
            let conns: Vec<ConnId> =
                self.links.keys().filter(|(n, _)| *n == i).map(|(_, c)| *c).collect();
            for conn in conns {
                let h = self.rng.next_u64();
                self.send(
                    i,
                    conn,
                    Message::Gossip { topic: TOPIC, data: payload(h, 0, false, 200) },
                );
                let ids: Vec<MessageId> = (0..64)
                    .map(|_| {
                        let mut id = [0u8; 20];
                        id[..8].copy_from_slice(&self.rng.next_u64().to_be_bytes());
                        id
                    })
                    .collect();
                self.send(i, conn, Message::IHave(ids.clone()));
                self.send(i, conn, Message::IWant(ids));
            }
        }
    }

    fn run(&mut self) -> Summary {
        let mut next_tick = TICK_MS;
        let mut next_block = 3_000;
        let mut next_attack = 1_000;
        let mut height = 0u64;
        while self.now < self.opts.duration_ms {
            let next_event = self.queue.keys().next().map(|k| k.0).unwrap_or(u64::MAX);
            let t = next_event.min(next_tick).min(next_block).min(next_attack);
            self.now = t.max(self.now);
            if let Some((start, end)) = self.opts.partition {
                let should = self.now >= start && self.now < end;
                if should != self.partitioned {
                    self.partitioned = should;
                    if should {
                        let cross: Vec<(usize, ConnId)> = self
                            .links
                            .iter()
                            .filter(|((a, _), (b, _))| self.crosses(*a, *b))
                            .map(|(k, _)| *k)
                            .collect();
                        for (a, c) in cross {
                            if self.links.contains_key(&(a, c)) {
                                self.close_link(a, c);
                                self.nodes[a].node.disconnected(c);
                                self.drain(a);
                            }
                        }
                    }
                }
            }
            if next_event <= t {
                let (_, d) = self.queue.pop_first().expect("present");
                self.deliver(d);
                continue;
            }
            if t == next_tick {
                next_tick += TICK_MS;
                for i in 0..self.nodes.len() {
                    self.nodes[i].node.tick(self.now);
                    self.drain(i);
                }
            }
            if t == next_attack {
                next_attack += 50;
                self.attack();
            }
            if t == next_block {
                next_block += self.opts.block_ms;
                height += 1;
                let sequencer = 0;
                let mut variants = 1;
                if self.opts.scenario == "equivocate" || self.opts.scenario == "all" {
                    variants = match height % 10 {
                        3 => 2,
                        7 => 7,
                        _ => 1,
                    };
                }
                for v in 0..variants {
                    let data = payload(height, v as u8, true, self.opts.payload);
                    let id = message_id(TOPIC, &data);
                    let app = &mut self.nodes[sequencer].app;
                    app.published.insert(id);
                    let at_height = app.per_height.entry(height).or_default();
                    if at_height.len() < MAX_PER_HEIGHT {
                        at_height.insert(id);
                        app.accepted.insert(id);
                        app.accepted_at.insert(id, self.now);
                        if v < MAX_PER_HEIGHT as u64 {
                            self.published.push((self.now, id, height));
                        }
                        self.nodes[sequencer].node.publish(TOPIC, data, self.now);
                        self.drain(sequencer);
                    }
                }
                // Stateless nodes (honest, not the sequencer) ask for the block's witness.
                if self.opts.scenario == "requests" || self.opts.scenario == "all" {
                    for i in 1..self.opts.nodes {
                        if i % 3 != 0 {
                            continue;
                        }
                        let id = self.nodes[i].node.request(
                            WITNESS_KIND,
                            height.to_be_bytes().to_vec(),
                            Target::Any,
                            self.now,
                        );
                        self.nodes[i]
                            .app
                            .pending_witness
                            .insert(height, (id, self.now, Vec::new()));
                        self.drain(i);
                    }
                }
            }
        }
        self.check()
    }

    fn check(&mut self) -> Summary {
        let mut latencies = Vec::new();
        let mut misses = 0u64;
        let settle = 20_000;
        let window_end = self.opts.duration_ms.saturating_sub(self.opts.deadline_ms);
        for &(at, id, height) in &self.published {
            if at < settle || at > window_end {
                continue;
            }
            if let Some((start, end)) = self.opts.partition
                && at + self.opts.deadline_ms >= start
                && at < end + settle
            {
                continue;
            }
            for i in 0..self.opts.nodes {
                match self.nodes[i].app.accepted_at.get(&id) {
                    Some(t) if t - at <= self.opts.deadline_ms => latencies.push(t - at),
                    // Of 6 or more variants at a height, each node accepts the first 5 it sees,
                    // so a node may lack a given variant (as base-consensus does); not a miss.
                    _ if self.nodes[i]
                        .app
                        .per_height
                        .get(&height)
                        .is_some_and(|s| s.len() >= MAX_PER_HEIGHT) => {}
                    other => {
                        misses += 1;
                        if misses <= 5 {
                            self.violations.push(format!(
                                "liveness: node {i} got message of height {height} published at {at} {}",
                                other.map_or_else(|| "never".to_string(), |t| format!("after {} ms", t - at))
                            ));
                        }
                    }
                }
            }
        }
        if misses > 5 {
            self.violations.push(format!("liveness: {misses} misses in total"));
        }
        let mut eclipse = None;
        if let Some(v) = self.victim {
            if self.opts.verbose {
                eprintln!("VICTIM {:?}", self.nodes[v].node);
            }
            let peers = self.nodes[v].node.peers();
            let honest_keys: HashSet<[u8; 33]> =
                self.nodes[..self.opts.nodes].iter().map(|n| n.key.serialize()).collect();
            let honest_out = peers
                .iter()
                .filter(|(id, _, d)| *d == Direction::Outbound && honest_keys.contains(id))
                .count();
            let honest_any = peers.iter().filter(|(id, _, _)| honest_keys.contains(id)).count();
            let out = peers.iter().filter(|(_, _, d)| *d == Direction::Outbound).count();
            if honest_any == 0 {
                let (mut h, mut a) = (0, 0);
                for e in self.nodes[v].node.table().entries() {
                    if honest_keys.contains(&e.key.serialize()) { h += 1 } else { a += 1 }
                }
                self.violations.push(format!(
                    "eclipse: victim {v} has no honest peer ({} peers, {out} outbound; table honest {h} attacker {a})",
                    peers.len()
                ));
            }
            eclipse = Some(Eclipse { honest_out, out, honest_any, peers: peers.len() });
        }
        let mut witness = (0, 0, 0);
        for n in &self.nodes[..self.opts.nodes] {
            witness.0 += n.app.witnesses_ok;
            witness.1 += n.app.witnesses_bad;
            witness.2 += n
                .app
                .pending_witness
                .values()
                .filter(|(_, t, _)| *t + 30_000 < self.opts.duration_ms)
                .count() as u64;
        }
        if witness.2 > 0 {
            self.violations
                .push(format!("requests: {} witness requests never answered correctly", witness.2));
        }
        latencies.sort_unstable();
        let pct = |p: usize| {
            latencies
                .get(latencies.len().saturating_sub(1).min(latencies.len() * p / 100))
                .copied()
                .unwrap_or(0)
        };
        let mut wl: Vec<u64> = self.nodes[..self.opts.nodes]
            .iter()
            .flat_map(|n| n.app.witness_latency.iter().copied())
            .collect();
        wl.sort_unstable();
        let mut dup = 0;
        let mut recv = 0;
        let mut bans = 0;
        for n in &self.nodes[..self.opts.nodes] {
            let s = n.node.stats();
            dup += s.gossip_duplicates;
            recv += s.gossip_received;
            bans += s.bans;
        }
        Summary {
            deliveries: latencies.len() as u64,
            p50: pct(50),
            p99: pct(99),
            max: latencies.last().copied().unwrap_or(0),
            misses,
            bytes: self.bytes,
            duplicates: dup,
            received: recv,
            bans,
            eclipse,
            witness_ok: witness.0,
            witness_bad: witness.1,
            witness_p50: wl.get(wl.len() / 2).copied().unwrap_or(0),
        }
    }
}

/// The victim's peers at the end of an eclipse run.
#[derive(Debug)]
#[allow(dead_code)]
struct Eclipse {
    honest_out: usize,
    out: usize,
    honest_any: usize,
    peers: usize,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Summary {
    deliveries: u64,
    p50: u64,
    p99: u64,
    max: u64,
    misses: u64,
    bytes: u64,
    duplicates: u64,
    received: u64,
    bans: u64,
    eclipse: Option<Eclipse>,
    witness_ok: u64,
    witness_bad: u64,
    witness_p50: u64,
}

fn parse(args: &[String]) -> Result<(Options, u64), String> {
    let mut opts = Options {
        scenario: "gossip".into(),
        seed: 1,
        nodes: 40,
        attackers: 0,
        attacker_groups: 4,
        duration_ms: 120_000,
        deadline_ms: 5_000,
        block_ms: 2_000,
        payload: 20_000,
        partition: None,
        fanout: 6,
        fanout_large: None,
        verbose: false,
    };
    let mut seeds = 1;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        let num = |v: String| v.parse::<u64>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--scenario" => opts.scenario = value()?,
            "--seed" => opts.seed = num(value()?)?,
            "--seeds" => seeds = num(value()?)?,
            "--nodes" => opts.nodes = num(value()?)? as usize,
            "--attackers" => opts.attackers = num(value()?)? as usize,
            "--attacker-groups" => opts.attacker_groups = num(value()?)? as usize,
            "--duration-ms" => opts.duration_ms = num(value()?)?,
            "--deadline-ms" => opts.deadline_ms = num(value()?)?,
            "--block-ms" => opts.block_ms = num(value()?)?,
            "--payload" => opts.payload = num(value()?)? as usize,
            "--fanout" => opts.fanout = num(value()?)? as usize,
            "--fanout-large" => opts.fanout_large = Some(num(value()?)? as usize),
            "--verbose" => opts.verbose = true,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    match opts.scenario.as_str() {
        "partition" => opts.partition = Some((40_000, 70_000)),
        "eclipse" | "flood" | "silent" | "requests" if opts.attackers == 0 => {
            opts.attackers = opts.nodes
        }
        "all" => {
            opts.partition = Some((40_000, 70_000));
            if opts.attackers == 0 {
                opts.attackers = opts.nodes / 2;
            }
        }
        _ => {}
    }
    Ok((opts, seeds))
}

/// Runs `sim --scenario <gossip|partition|eclipse|flood|silent|requests|equivocate|all>
/// [--seed S] [--seeds N] [--nodes N] [--attackers N] [--attacker-groups G] [--duration-ms T]`.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let (opts, seeds) = parse(args)?;
    let mut failed = 0;
    for s in 0..seeds {
        let mut o = opts.clone();
        o.seed = opts.seed + s;
        let mut sim = Sim::new(o.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sim.run()));
        match result {
            Ok(summary) => {
                println!("scenario={} seed={} {:?}", o.scenario, o.seed, summary);
                if !sim.violations.is_empty() {
                    failed += 1;
                    for v in sim.violations.iter().take(if o.verbose { usize::MAX } else { 10 }) {
                        println!("  VIOLATION {v}");
                    }
                }
            }
            Err(_) => {
                failed += 1;
                println!("scenario={} seed={} PANIC", o.scenario, o.seed);
            }
        }
    }
    if failed > 0 { Err(format!("{failed} of {seeds} seeds failed")) } else { Ok(()) }
}
