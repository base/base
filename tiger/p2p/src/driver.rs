//! Runs a [`Node`] over TCP on std threads: one core thread that owns the node, one listener,
//! and per connection a reader (handshake, decryption, reassembly, decoding) and a writer
//! (chunking, priority interleaving, encryption). Crypto runs on the connection threads, so the
//! core thread does only protocol logic.
//!
//! Bounds: at most [`MAX_PENDING_HANDSHAKES`] handshakes at once and
//! [`MAX_PENDING_HANDSHAKES_PER_IP`] per IP, a per-IP rate of new connections, a handshake
//! deadline, a deadline on every encrypted frame once its first byte arrived (slow senders), an
//! idle read timeout, and a cap on each connection's queued outgoing bytes (slow readers).

use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use ring::rand::{SecureRandom, SystemRandom};
use secp256k1::{PublicKey, SecretKey};

use crate::{
    node::{
        Config, ConnId, Direction, Event, Node, Output, Penalty, RequestId, Target, Validation,
    },
    noise::{
        ACT_ONE_LEN, ACT_THREE_LEN, ACT_TWO_LEN, Initiator, LENGTH_HEADER_LEN, Receiver, Responder,
        Sender, TAG_LEN,
    },
    table::PeerId,
    wire::{
        DisconnectReason, FLAG_BULK, FLAG_START, Message, MessageId, Reassembler, ResponseCode, chunk,
    },
};

/// Handshakes in progress at once.
pub const MAX_PENDING_HANDSHAKES: usize = 64;
/// Handshakes in progress from one IP (loopback: 16, for local devnets).
pub const MAX_PENDING_HANDSHAKES_PER_IP: usize = 2;
/// The whole handshake must finish within this.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Once its first byte arrived, an encrypted frame must arrive whole within this.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection that sends nothing for this long is closed (peers ping every 15 s).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// The longest a message may take from its first chunk to its last, so a peer trickling chunks
/// cannot hold a partial message (up to its type's limit) open forever. A 32 MiB response
/// needs about 0.6 MB/s.
pub const MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);
/// A connection whose queued outgoing priority bytes exceed this is closed.
pub const MAX_QUEUED_PRIORITY: usize = 16 * 1024 * 1024;
/// A connection whose queued outgoing bulk bytes exceed this is closed.
pub const MAX_QUEUED_BULK: usize = 128 * 1024 * 1024;
/// New inbound connections per IP per second (burst twice that).
const CONNECTIONS_PER_IP_PER_SECOND: f64 = 5.0;
const CORE_TICK: Duration = Duration::from_millis(20);

/// Byte and message counters for one direction, shared with the caller for measurements.
#[derive(Debug, Default)]
pub struct Traffic {
    /// Bytes written to sockets, handshakes included.
    pub bytes_out: AtomicU64,
    /// Bytes read from sockets, handshakes included.
    pub bytes_in: AtomicU64,
}

enum Input {
    Connected {
        conn: ConnId,
        key: PublicKey,
        addr: SocketAddr,
        direction: Direction,
        outbox: Arc<Outbox>,
    },
    Message {
        conn: ConnId,
        message: Message,
    },
    Violation {
        conn: ConnId,
    },
    Closed {
        conn: ConnId,
    },
    DialFailed {
        key: PublicKey,
    },
    Command(Command),
}

enum Command {
    Publish { topic: u8, data: Vec<u8>, reply: mpsc::SyncSender<MessageId> },
    Validated { id: MessageId, validation: Validation },
    Request { kind: u16, body: Vec<u8>, target: Target, reply: mpsc::SyncSender<RequestId> },
    Respond { peer: PeerId, id: u32, kind: u16, code: ResponseCode, body: Vec<u8> },
    Notify { peer: Option<PeerId>, kind: u16, body: Vec<u8> },
    Report { peer: PeerId, penalty: Penalty },
    SetHead { number: u64, hash: [u8; 32] },
    Peers { reply: mpsc::SyncSender<Vec<(PeerId, SocketAddr, Direction)>> },
    Score { peer: PeerId, reply: mpsc::SyncSender<Option<i32>> },
    Shutdown,
}

/// A connection's outgoing queues: priority chunks are always written before bulk chunks, so a
/// bulk transfer delays gossip by at most one chunk.
struct Outbox {
    state: Mutex<OutboxState>,
    ready: Condvar,
}

#[derive(Default)]
struct OutboxState {
    priority: VecDeque<Vec<u8>>,
    bulk: VecDeque<Vec<u8>>,
    priority_bytes: usize,
    bulk_bytes: usize,
    closing: Option<DisconnectReason>,
}

impl Outbox {
    fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(OutboxState::default()), ready: Condvar::new() })
    }

    /// Queues a message; returns false when the queue is over its cap (the reader is too slow).
    fn push(&self, message: &Message) -> bool {
        let bulk = message.is_bulk();
        let body = message.encode_body();
        let mut state = self.state.lock().expect("outbox lock");
        if state.closing.is_some() {
            return true;
        }
        let over = if bulk {
            state.bulk_bytes + body.len() > MAX_QUEUED_BULK
        } else {
            state.priority_bytes + body.len() > MAX_QUEUED_PRIORITY
        };
        if over {
            return false;
        }
        chunk(message.type_byte(), &body, bulk, |c| {
            if bulk {
                state.bulk_bytes += c.len();
                state.bulk.push_back(c);
            } else {
                state.priority_bytes += c.len();
                state.priority.push_back(c);
            }
        });
        self.ready.notify_one();
        true
    }

    fn close(&self, reason: DisconnectReason) {
        let mut state = self.state.lock().expect("outbox lock");
        if state.closing.is_none() {
            state.closing = Some(reason);
            self.ready.notify_one();
        }
    }

    /// The next chunk to write, or the reason to close once the queues hold nothing to send.
    fn next(&self) -> Result<Vec<u8>, DisconnectReason> {
        let mut state = self.state.lock().expect("outbox lock");
        loop {
            if let Some(reason) = state.closing {
                return Err(reason);
            }
            if let Some(c) = state.priority.pop_front() {
                state.priority_bytes -= c.len();
                return Ok(c);
            }
            if let Some(c) = state.bulk.pop_front() {
                state.bulk_bytes -= c.len();
                return Ok(c);
            }
            state = self.ready.wait(state).expect("outbox lock");
        }
    }
}

/// The application's handle on a running node.
#[derive(Debug, Clone)]
pub struct Handle {
    commands: mpsc::Sender<Input>,
    local_addr: Option<SocketAddr>,
    traffic: Arc<Traffic>,
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Input")
    }
}

impl Handle {
    /// The address the listener is bound to.
    pub const fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Socket byte counters.
    pub fn traffic(&self) -> &Traffic {
        &self.traffic
    }

    fn command(&self, command: Command) {
        // The core thread outlives every handle until shutdown; a send after it is a no-op.
        let _ = self.commands.send(Input::Command(command));
    }

    /// Publishes a gossip message of our own; returns its id.
    pub fn publish(&self, topic: u8, data: Vec<u8>) -> MessageId {
        let (reply, rx) = mpsc::sync_channel(1);
        self.command(Command::Publish { topic, data, reply });
        rx.recv().unwrap_or([0; 20])
    }

    /// Reports the application's verdict on a gossip message.
    pub fn validated(&self, id: MessageId, validation: Validation) {
        self.command(Command::Validated { id, validation });
    }

    /// Sends a request; the answer arrives as [`Event::Response`] with the returned id.
    pub fn request(&self, kind: u16, body: Vec<u8>, target: Target) -> RequestId {
        let (reply, rx) = mpsc::sync_channel(1);
        self.command(Command::Request { kind, body, target, reply });
        rx.recv().unwrap_or(0)
    }

    /// Answers a peer's request.
    pub fn respond(&self, peer: PeerId, id: u32, kind: u16, code: ResponseCode, body: Vec<u8>) {
        self.command(Command::Respond { peer, id, kind, code, body });
    }

    /// Sends a one-hop notice to one peer or all.
    pub fn notify(&self, peer: Option<PeerId>, kind: u16, body: Vec<u8>) {
        self.command(Command::Notify { peer, kind, body });
    }

    /// Reports a peer's bad response.
    pub fn report(&self, peer: PeerId, penalty: Penalty) {
        self.command(Command::Report { peer, penalty });
    }

    /// Sets the head reported in Status.
    pub fn set_head(&self, number: u64, hash: [u8; 32]) {
        self.command(Command::SetHead { number, hash });
    }

    /// The ready peers.
    pub fn peers(&self) -> Vec<(PeerId, SocketAddr, Direction)> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.command(Command::Peers { reply });
        rx.recv().unwrap_or_default()
    }

    /// A ready peer's score after recovery (none: not a ready peer), for tests and measurements.
    pub fn score(&self, peer: PeerId) -> Option<i32> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.command(Command::Score { peer, reply });
        rx.recv().ok().flatten()
    }

    /// Stops the core thread and closes every connection.
    pub fn shutdown(&self) {
        self.command(Command::Shutdown);
    }
}

/// Fresh random secret key from the operating system.
pub fn random_secret() -> SecretKey {
    let rng = SystemRandom::new();
    loop {
        let mut bytes = [0u8; 32];
        rng.fill(&mut bytes).expect("operating system randomness is available");
        if let Ok(key) = SecretKey::from_byte_array(bytes) {
            return key;
        }
    }
}

fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    SystemRandom::new().fill(&mut bytes).expect("operating system randomness is available");
    u64::from_le_bytes(bytes)
}

struct Shared {
    secret: SecretKey,
    prologue: Vec<u8>,
    inputs: mpsc::Sender<Input>,
    next_conn: AtomicU64,
    pending: Mutex<HashMap<IpAddr, usize>>,
    rates: Mutex<HashMap<IpAddr, (f64, Instant)>>,
    limits: Config,
    traffic: Arc<Traffic>,
    stopped: AtomicBool,
}

/// Starts a node listening on `listen` (none: dial only). Events arrive on the returned
/// receiver; the [`Handle`] sends commands. The config's seed is replaced with OS randomness
/// unless `deterministic_seed` is set (tests).
pub fn start(
    mut config: Config,
    listen: Option<SocketAddr>,
    deterministic_seed: bool,
) -> io::Result<(Handle, mpsc::Receiver<Event>)> {
    if !deterministic_seed {
        config.seed = random_u64();
    }
    let listener = listen.map(TcpListener::bind).transpose()?;
    let local_addr = listener.as_ref().map(TcpListener::local_addr).transpose()?;
    if config.advertise.is_none() {
        config.advertise = local_addr;
    }
    let (inputs, input_rx) = mpsc::channel();
    let (events, event_rx) = mpsc::channel();
    let traffic = Arc::new(Traffic::default());
    let shared = Arc::new(Shared {
        secret: config.secret,
        prologue: config.network.prologue(),
        inputs: inputs.clone(),
        next_conn: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        rates: Mutex::new(HashMap::new()),
        limits: config.clone(),
        traffic: traffic.clone(),
        stopped: AtomicBool::new(false),
    });
    if let Some(listener) = listener {
        let shared = shared.clone();
        thread::Builder::new()
            .name("tp2p-listen".into())
            .spawn(move || listen_loop(&listener, &shared))?;
    }
    let core_shared = shared;
    thread::Builder::new()
        .name("tp2p-core".into())
        .spawn(move || core_loop(config, &input_rx, &events, &core_shared))?;
    Ok((Handle { commands: inputs, local_addr, traffic }, event_rx))
}

fn now_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64 + 1
}

fn core_loop(
    config: Config,
    inputs: &mpsc::Receiver<Input>,
    events: &mpsc::Sender<Event>,
    shared: &Arc<Shared>,
) {
    let start = Instant::now();
    let mut node = Node::new(config, now_ms(start));
    let mut outboxes: HashMap<ConnId, Arc<Outbox>> = HashMap::new();
    let mut last_tick = Instant::now();
    loop {
        let input = inputs.recv_timeout(CORE_TICK);
        let now = now_ms(start);
        match input {
            Ok(Input::Connected { conn, key, addr, direction, outbox }) => {
                match node.connected(conn, key, addr, direction, now) {
                    Ok(()) => {
                        outboxes.insert(conn, outbox);
                    }
                    Err(reason) => outbox.close(reason),
                }
            }
            Ok(Input::Message { conn, message }) => node.received(conn, message, now),
            Ok(Input::Violation { conn }) => node.violation(conn),
            Ok(Input::Closed { conn }) => {
                outboxes.remove(&conn);
                node.disconnected(conn);
            }
            Ok(Input::DialFailed { key }) => node.dial_failed(&key),
            Ok(Input::Command(command)) => match command {
                Command::Publish { topic, data, reply } => {
                    let _ = reply.send(node.publish(topic, data, now));
                }
                Command::Validated { id, validation } => node.validated(&id, validation, now),
                Command::Request { kind, body, target, reply } => {
                    let _ = reply.send(node.request(kind, body, target, now));
                }
                Command::Respond { peer, id, kind, code, body } => {
                    node.respond(&peer, id, kind, code, body)
                }
                Command::Notify { peer, kind, body } => node.notify(peer.as_ref(), kind, &body),
                Command::Report { peer, penalty } => node.report(&peer, penalty, now),
                Command::SetHead { number, hash } => node.set_head(number, hash),
                Command::Peers { reply } => {
                    let _ = reply.send(node.peers());
                }
                Command::Score { peer, reply } => {
                    let _ = reply.send(node.score(&peer, now));
                }
                Command::Shutdown => {
                    shared.stopped.store(true, Ordering::SeqCst);
                    for outbox in outboxes.values() {
                        outbox.close(DisconnectReason::Requested);
                    }
                    return;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if last_tick.elapsed() >= CORE_TICK {
            last_tick = Instant::now();
            node.tick(now);
        }
        while let Some(output) = node.poll() {
            match output {
                Output::Send { conn, message } => {
                    if let Some(outbox) = outboxes.get(&conn)
                        && !outbox.push(&message)
                    {
                        outbox.close(DisconnectReason::Timeout);
                    }
                }
                Output::Close { conn, reason } => {
                    if let Some(outbox) = outboxes.remove(&conn) {
                        outbox.close(reason);
                    }
                }
                Output::Dial { key, addr } => {
                    let shared = shared.clone();
                    let spawned = thread::Builder::new()
                        .name("tp2p-dial".into())
                        .spawn(move || dial(&shared, key, addr));
                    if spawned.is_err() {
                        node.dial_failed(&key);
                    }
                }
                Output::Event(event) => {
                    let _ = events.send(event);
                }
            }
        }
    }
}

fn listen_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let Ok(addr) = stream.peer_addr() else { continue };
        if !admit(shared, addr.ip()) {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let shared2 = shared.clone();
        let spawned = thread::Builder::new().name("tp2p-conn".into()).spawn(move || {
            let result = accept(&shared2, stream, addr);
            release(&shared2, addr.ip());
            if let Ok((conn, receiver, stream)) = result {
                read_loop(&shared2, conn, receiver, stream);
            }
        });
        if spawned.is_err() {
            release(shared, addr.ip());
        }
    }
}

/// Admits a new inbound connection under the handshake limits, or refuses it before any crypto.
fn admit(shared: &Shared, ip: IpAddr) -> bool {
    {
        let mut rates = shared.rates.lock().expect("rates lock");
        if rates.len() > 65_536 {
            rates.clear();
        }
        let burst = CONNECTIONS_PER_IP_PER_SECOND * 2.0;
        let entry = rates.entry(ip).or_insert((burst, Instant::now()));
        entry.0 =
            (entry.0 + entry.1.elapsed().as_secs_f64() * CONNECTIONS_PER_IP_PER_SECOND).min(burst);
        entry.1 = Instant::now();
        if entry.0 < 1.0 && !ip.is_loopback() {
            return false;
        }
        entry.0 -= 1.0;
    }
    let mut pending = shared.pending.lock().expect("pending lock");
    let total: usize = pending.values().sum();
    let per_ip = if ip.is_loopback() { 16 } else { MAX_PENDING_HANDSHAKES_PER_IP };
    let count = pending.entry(ip).or_insert(0);
    if total >= MAX_PENDING_HANDSHAKES || *count >= per_ip {
        return false;
    }
    *count += 1;
    true
}

fn release(shared: &Shared, ip: IpAddr) {
    let mut pending = shared.pending.lock().expect("pending lock");
    if let Some(count) = pending.get_mut(&ip) {
        *count -= 1;
        if *count == 0 {
            pending.remove(&ip);
        }
    }
}

/// Reads exactly `buf.len()` bytes before `deadline`.
fn read_exact_by(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
    traffic: &Traffic,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(left))?;
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                filled += n;
                traffic.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_all(stream: &mut TcpStream, buf: &[u8], traffic: &Traffic) -> io::Result<()> {
    stream.write_all(buf)?;
    traffic.bytes_out.fetch_add(buf.len() as u64, Ordering::Relaxed);
    Ok(())
}

fn accept(
    shared: &Arc<Shared>,
    mut stream: TcpStream,
    addr: SocketAddr,
) -> io::Result<(ConnId, Receiver, TcpStream)> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut act1 = [0u8; ACT_ONE_LEN];
    read_exact_by(&mut stream, &mut act1, deadline, &shared.traffic)?;
    let responder = Responder::new(shared.secret, &shared.prologue, random_secret());
    let (awaiting, act2) =
        responder.act_one(&act1).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_all(&mut stream, &act2, &shared.traffic)?;
    let mut act3 = [0u8; ACT_THREE_LEN];
    read_exact_by(&mut stream, &mut act3, deadline, &shared.traffic)?;
    let (key, session) =
        awaiting.act_three(&act3).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    established(shared, stream, key, addr, Direction::Inbound, session.sender, session.receiver)
}

fn dial(shared: &Arc<Shared>, key: PublicKey, addr: SocketAddr) {
    let result = (|| {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let mut stream = TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT)?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let (initiator, act1) =
            Initiator::new(shared.secret, &key, &shared.prologue, random_secret());
        write_all(&mut stream, &act1, &shared.traffic)?;
        let mut act2 = [0u8; ACT_TWO_LEN];
        read_exact_by(&mut stream, &mut act2, deadline, &shared.traffic)?;
        let (act3, session) =
            initiator.act_two(&act2).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_all(&mut stream, &act3, &shared.traffic)?;
        established(
            shared,
            stream,
            key,
            addr,
            Direction::Outbound,
            session.sender,
            session.receiver,
        )
    })();
    match result {
        Ok((conn, receiver, stream)) => read_loop(shared, conn, receiver, stream),
        Err(_) => {
            let _ = shared.inputs.send(Input::DialFailed { key });
        }
    }
}

/// Starts the writer and registers the connection with the core.
fn established(
    shared: &Arc<Shared>,
    stream: TcpStream,
    key: PublicKey,
    addr: SocketAddr,
    direction: Direction,
    sender: Sender,
    receiver: Receiver,
) -> io::Result<(ConnId, Receiver, TcpStream)> {
    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let outbox = Outbox::new();
    let mut writer_stream = stream.try_clone()?;
    writer_stream.set_write_timeout(Some(IDLE_TIMEOUT))?;
    let writer_outbox = outbox.clone();
    let traffic = shared.traffic.clone();
    thread::Builder::new().name("tp2p-write".into()).spawn(move || {
        write_loop(&mut writer_stream, sender, &writer_outbox, &traffic);
    })?;
    shared
        .inputs
        .send(Input::Connected { conn, key, addr, direction, outbox })
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    Ok((conn, receiver, stream))
}

fn write_loop(stream: &mut TcpStream, mut sender: Sender, outbox: &Outbox, traffic: &Traffic) {
    let mut wire = Vec::with_capacity(70_000);
    loop {
        match outbox.next() {
            Ok(c) => {
                wire.clear();
                sender.encrypt(&c, &mut wire);
                if write_all(stream, &wire, traffic).is_err() {
                    break;
                }
            }
            Err(reason) => {
                let message = Message::Disconnect(reason);
                chunk(message.type_byte(), &message.encode_body(), false, |c| {
                    wire.clear();
                    sender.encrypt(&c, &mut wire);
                    let _ = write_all(stream, &wire, traffic);
                });
                break;
            }
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn read_loop(shared: &Arc<Shared>, conn: ConnId, mut receiver: Receiver, mut stream: TcpStream) {
    let limits = &shared.limits;
    let mut reassembler = Reassembler::default();
    let mut body = Vec::with_capacity(70_000);
    // Per stream (priority, bulk): when the message in progress is due.
    let mut message_due: [Option<Instant>; 2] = [None, None];
    let outcome: Result<(), bool> = (|| loop {
        let mut header = [0u8; LENGTH_HEADER_LEN];
        // The first byte of a frame may take up to the idle timeout, or until a started message
        // is due; the rest of the frame must follow within the frame timeout.
        let idle = Instant::now() + IDLE_TIMEOUT;
        read_exact_by(
            &mut stream,
            &mut header[..1],
            message_due.iter().flatten().fold(idle, |at, due| at.min(*due)),
            &shared.traffic,
        )
        .map_err(|_| false)?;
        let deadline = Instant::now() + FRAME_TIMEOUT;
        read_exact_by(&mut stream, &mut header[1..], deadline, &shared.traffic)
            .map_err(|_| false)?;
        let len = receiver.decrypt_length(&header).map_err(|_| true)?;
        body.resize(len + TAG_LEN, 0);
        read_exact_by(&mut stream, &mut body, deadline, &shared.traffic).map_err(|_| false)?;
        let plain = receiver.decrypt_body(&mut body).map_err(|_| true)?;
        let flags = plain.first().copied().unwrap_or(0);
        let complete = reassembler.push(plain, |t| limits.message_limit(t)).map_err(|_| true)?;
        let due = &mut message_due[usize::from(flags & FLAG_BULK != 0)];
        if flags & FLAG_START != 0 {
            *due = Some(Instant::now() + MESSAGE_TIMEOUT);
        }
        if complete.is_some() {
            *due = None;
        }
        if message_due.iter().flatten().any(|due| Instant::now() > *due) {
            return Err(false);
        }
        let Some((type_byte, message_body)) = complete else {
            continue;
        };
        let message = Message::decode(type_byte, &message_body).map_err(|_| true)?;
        shared.inputs.send(Input::Message { conn, message }).map_err(|_| false)?;
    })();
    if outcome == Err(true) {
        let _ = shared.inputs.send(Input::Violation { conn });
    }
    let _ = stream.shutdown(Shutdown::Both);
    let _ = shared.inputs.send(Input::Closed { conn });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Network, RequestKind};
    use secp256k1::SECP256K1;

    const NET: Network = Network { chain_id: 1, genesis: [0; 32], layer: Network::EXECUTION };

    fn config(i: u8) -> Config {
        let mut c = Config::new(NET, SecretKey::from_byte_array([i; 32]).unwrap(), u64::from(i));
        c.capabilities = 1;
        c.requests.push(RequestKind {
            kind: 7,
            capability: 1,
            max_request: 100,
            max_response: 8 * 1024 * 1024,
            per_peer_per_second: 100,
            timeout_ms: 5000,
        });
        c
    }

    fn wait<T>(rx: &mpsc::Receiver<Event>, mut f: impl FnMut(Event) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("event in time");
            if let Some(t) = f(event) {
                return t;
            }
        }
    }

    /// Two nodes over loopback: connect through a bootnode entry, exchange a 5 MiB response
    /// while gossip flows, and validate gossip.
    #[test]
    fn loopback_request_and_gossip() {
        let (server, server_events) =
            start(config(1), Some("127.0.0.1:0".parse().unwrap()), true).unwrap();
        let server_key =
            PublicKey::from_secret_key(SECP256K1, &SecretKey::from_byte_array([1; 32]).unwrap());
        let mut c = config(2);
        c.bootnodes.push((server_key, server.local_addr().unwrap()));
        let (client, client_events) = start(c, Some("127.0.0.1:0".parse().unwrap()), true).unwrap();
        wait(&client_events, |e| matches!(e, Event::PeerReady { .. }).then_some(()));
        wait(&server_events, |e| matches!(e, Event::PeerReady { .. }).then_some(()));

        let request = client.request(7, vec![1], Target::Any);
        let (peer, id) = wait(&server_events, |e| match e {
            Event::Request { peer, id, .. } => Some((peer, id)),
            _ => None,
        });
        let big = vec![0xab; 5 * 1024 * 1024];
        server.respond(peer, id, 7, ResponseCode::Ok, big.clone());
        let gossip_id = server.publish(3, vec![9; 1000]);
        let mut got_gossip_first = None;
        let body = wait(&client_events, |e| match e {
            Event::Gossip { id, .. } => {
                got_gossip_first.get_or_insert(true);
                client.validated(id, Validation::Accept);
                None
            }
            Event::Response { request: r, result, .. } if r == request => {
                got_gossip_first.get_or_insert(false);
                Some(result.unwrap())
            }
            _ => None,
        });
        assert_eq!(body, big);
        assert_eq!(got_gossip_first, Some(true), "gossip overtakes the bulk response");
        let _ = gossip_id;
        assert!(client.traffic().bytes_in.load(Ordering::Relaxed) > 5 * 1024 * 1024);
        client.shutdown();
        server.shutdown();
    }

    /// A peer that opens a TCP connection and dribbles the handshake is dropped at the deadline,
    /// and one that sends garbage is dropped at once.
    #[test]
    fn slow_and_garbage_handshakes_are_dropped() {
        let (server, _events) =
            start(config(3), Some("127.0.0.1:0".parse().unwrap()), true).unwrap();
        let addr = server.local_addr().unwrap();
        let mut slow = TcpStream::connect(addr).unwrap();
        slow.write_all(&[0u8; 10]).unwrap();
        let started = Instant::now();
        slow.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = [0u8; 1];
        let n = slow.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0);
        assert!(
            started.elapsed() >= Duration::from_secs(4)
                && started.elapsed() < Duration::from_secs(8)
        );

        let mut garbage = TcpStream::connect(addr).unwrap();
        garbage.write_all(&[0x55; ACT_ONE_LEN]).unwrap();
        garbage.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let started = Instant::now();
        assert_eq!(garbage.read(&mut buf).unwrap_or(0), 0);
        assert!(started.elapsed() < Duration::from_secs(1));
        server.shutdown();
    }
}
