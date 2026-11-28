# base-tiger-p2p

tp2p is an encrypted peer-to-peer network for Base nodes. It can carry the execution layer's
traffic (tiger's block, header and witness sync, in place of devp2p) and the consensus layer's
gossip of sequencer-signed unsafe payloads (in place of libp2p gossipsub), each on its own
network. It is optional and off by default in every binary that links it. It uses std threads
and no async runtime. Its only dependencies are `ring` and `secp256k1`, both already in tiger's
and the base workspace's trees, so linking it adds no crate.

## Layers

| Module | What it does |
| --- | --- |
| `noise` | Handshake and transport encryption: `Noise_XK_secp256k1_ChaChaPoly_SHA256`, exactly as BOLT 8 specifies, with tp2p's prologue. |
| `wire` | Messages and their binary encoding; chunking of messages into Noise messages on two streams. |
| `table` | Signed peer records from peer exchange, and whom to dial. |
| `node` | The protocol as a sans-IO state machine: Status, ping, peer exchange, requests, gossip, scores and bans. |
| `driver` | Runs a `Node` over TCP: listener, core thread, and a reader and writer thread per connection. |

The simulator (`etc/tools/tiger-p2p-fuzz`, `sim`) runs many `Node`s against a simulated network
deterministically from a seed.

## Handshake

The handshake is BOLT 8's `Noise_XK_secp256k1_ChaChaPoly_SHA256`, unchanged except for the
prologue:

- **Acts.** Act One is `-> e, es`, 50 bytes. Act Two is `<- e, ee`, 50 bytes. Act Three is
  `-> s, se`, 66 bytes. Each starts with version byte 0.
- **Keys.** Static keys are secp256k1. ECDH is SHA-256 of the compressed shared point. HKDF is
  RFC 5869 over SHA-256 with an empty info. The AEAD is ChaCha20-Poly1305 with a nonce of 32 zero
  bits followed by a 64-bit little-endian counter. All of it comes from `ring` and `secp256k1`.
- **Prologue.** Instead of `lightning`, the prologue is 47 bytes:
  `"tp2p/1" || chain_id (u64, big-endian) || genesis hash (32) || layer (1: execution, 2: consensus)`.
  A peer on another chain, genesis or layer fails Act One's tag. The network id is SHA-256 of the
  prologue.
- **Transport.** Each message is sent as an encrypted 2-byte length (18 bytes with its tag)
  followed by the encrypted message of at most 65,535 bytes. Each direction's key rotates after
  1,000 uses: `ck', k' = HKDF(ck, k)`.
- **Test vectors.** BOLT 8 Appendix A passes with the `lightning` prologue (`noise.rs` tests): the
  initiator and responder handshakes, every failure vector, and the 1,002 transport messages
  across two key rotations.

XK suits tp2p because a dialer always knows the key of the peer it dials, from a bootnode entry
or a signed record. The responder's key never crosses the wire, and the initiator's key is sent
encrypted with forward secrecy.

**Replay.**
- A replayed Act One costs the responder one ECDH and one 50-byte reply. It can never complete:
  Act Three needs the initiator's static and ephemeral secrets.
- A replayed or reordered transport message fails its tag, because every message uses the next
  nonce.

**Rate limits, applied before any crypto.**
- At most 64 handshakes in flight.
- At most 2 handshakes in flight per IP (16 on loopback, for local devnets).
- At most 5 new connections per IP per second.
- A 5-second deadline for the whole handshake.

## Framing

A tp2p message is a type byte and a body (`wire.rs`). It is split into chunks of up to one Noise
message:

- **Flags byte.** `0x80` marks the bulk stream, `0x01` marks the start chunk and `0x02` marks the
  end chunk.
- **Start chunk.** After the flags, it carries the type byte and the body length (`u32`).

Requests and responses travel on the bulk stream; everything else travels on the priority
stream. The writer always sends priority chunks first, so a large response delays gossip by at
most one 64 KiB chunk.

The reader refuses a declared length above the limit for its type before buffering anything.
Each limit comes from the configured gossip size and request and notice kinds. Decoders check
counts against the bytes present and refuse trailing bytes.

Slow senders: a frame must arrive within 10 s of its first byte, a connection with nothing to
read for 45 s is closed, and each message must arrive in full within 60 s of its start chunk,
per stream, so trickled chunks cannot keep a partial message buffered.

## Messages

| Type | Message | Notes |
| --- | --- | --- |
| `0x00` | `Status` | Sent first in each direction: version, capabilities, head, listen port. |
| `0x01`, `0x02` | `Ping`, `Pong` | A ping every 15 s; 20 s of silence after a ping closes the connection. |
| `0x03` | `Disconnect` | The reason. |
| `0x04`, `0x05` | `GetPeers`, `Peers` | Signed records, at most 64. |
| `0x10`, `0x11` | `Request`, `Response` | Application kinds with per-kind size, rate and timeout limits. |
| `0x20` | `Gossip` | Forwarded only after the application accepts it. |
| `0x21`, `0x22`, `0x24` | `IHave`, `IWant`, `IDontWant` | Gossip by id. |
| `0x23` | `Notify` | A one-hop application notice, such as a block announcement. |

## Gossip

A gossip message's id is the first 20 bytes of SHA-256(topic || data).

- **Receiving.** On arrival, a message goes to the application. If it is at least 1 KiB, an
  `IDontWant` goes to the other peers at the same time.
- **Verdicts.** The application answers Accept, Ignore or Reject, as in gossipsub's application
  validation.
  - Accept: the message goes in full to `gossip_fanout` peers that lack it (6 by default, or 1
    for messages of 16 KiB or more), and its id goes to the rest.
  - Reject: the sender loses 25 points.
- **Fetching by id.** A peer that hears only the id asks for the message with `IWant`. After
  500 ms without an answer it asks the next peer that announced it.
- **Duplicates and amplification.** Each node delivers each id to its application once. It sends
  each accepted message to a peer at most once, so repeated `IWant`s amplify nothing.

**Fanout for large messages.** In the simulator (40 nodes, 100 KB messages, 100 Mbit/s uplinks,
5–100 ms links), pushing every message to 6 peers delivered about 6.7 copies to each node.
Announcing them instead cut the bytes 6.5 times, at about 10% more median latency. A fanout of 1
for large messages sits between the two. Measurements against gossipsub are in the fuzz tool's
README.

## Requests

Request kinds come from configuration, each with:
- a capability bit;
- a request size limit and a response size limit;
- a per-peer rate (token bucket);
- a timeout.

How requests are handled:
- **Choosing a peer.** A request goes to the ready peer with the capability that has the fewest
  requests in flight, then the best score, then the lowest round-trip time.
- **In flight.** At most 32 requests per peer in each direction, and 64 inbound.
- **Over the rate.** The node answers `Busy` and costs the peer one point.
- **Failures.** A timeout costs 5 points. An answer to nothing we asked closes the connection.

## Scores and bans

A peer's score starts at 0 and recovers 1 point per second toward 0. Penalties:

| Cause | Points |
| --- | --- |
| Rejected gossip | 25 |
| Bad response, as the application reports | 50 |
| Timeout | 5 |
| Rate-limit hit | 1 |

At −100 the peer is disconnected and its key and IP are banned for an hour. Trusted peers are
never penalized. A protocol violation (bad chunk, undecodable message, wrong order) closes the
connection at once.

## Discovery

Discovery is peer exchange, seeded from bootnodes.

- **Records.** A record is a peer's key, address, sequence number and capabilities, signed by
  that peer over the network id, so it cannot be forged or replayed onto another network. A peer
  sends its own record after `Status`. Unsolicited lists are ignored.
- **Bootstrapping.** A node asks every peer it dials for records once, and every 30 s it asks a
  random outbound peer (any peer, if it has no outbound one).
- **A full node still refers.** A node at its inbound limit still accepts up to 8 extra
  connections. Each gets one peer-exchange answer and is then closed, so a refused node still
  learns where to go.

`table.rs` follows Bitcoin Core's address manager in a smaller form:

- **New buckets.** A record heard from a source goes into one of 64 buckets, chosen by the
  source's network group and the record's group. A group is the /16 of a public IPv4 address,
  the /32 of a public IPv6 address, or the whole socket address for a loopback or private one. So
  one source group reaches at most 8 buckets of 8 entries. A new record evicts only an entry that
  failed 3 dials without ever succeeding.
- **Tried buckets.** A peer we connected to moves to a tried bucket chosen by its own group.
- **Outbound dials.** These go to bootnodes first, then alternate between tried and new entries,
  with at most one peer per group (dials in flight count too). Failed dials back off from 10 s,
  doubling, up to 10 minutes.
- **Inbound cap per group.** A node accepts at most 4 inbound peers per group.
- **Anchors and saved peers.** With `Config::peers_file` set, the driver saves its anchors and
  its tried table every minute and at shutdown, and loads them at start, as Bitcoin Core does
  with `anchors.dat` and `peers.dat`. Anchors are the (at most 4) ready outbound peers connected
  longest. After a restart they are dialed right after bootnodes, once each, and the tried table
  comes back as it was. Records are verified again when they are loaded. tiger keeps the file at
  `<datadir>/tp2p-peers`; base-consensus keeps it where `--p2p.tp2p.peers-path` says, and keeps
  none without that flag.

### Eclipse and sybil limits

- Sybils within one group gain nothing over one node: 4 inbound slots, 1 outbound slot, 8 new
  buckets.
- An attacker with addresses in G groups can hold at most G outbound slots, and at most 4·G
  inbound slots.
- An attacker with more groups than the node has outbound slots (8 by default), who also answers
  the node's peer exchanges, can fill most of its outbound slots over time.

These measurements come from the simulator's `eclipse` scenario: 40 honest nodes and 40 attackers.
The attackers answer every peer exchange with attacker records only, dial the victim, and relay
nothing. 12 seeds per row:

| Attacker groups | Victim outbound, of which honest (mean) | Honest peers in any direction (mean) | Runs with no honest outbound | Gossip misses |
| --- | --- | --- | --- | --- |
| 2 | 5 / 3.2 | 11.1 | 6 | 0 |
| 4 | 6 / 3.3 | 11.5 | 6 | 0 |
| 8 | 8 / 3.5 | 7.4 | 3 | 0 |
| 16 | 8 / 1.7 | 5.6 | 3 | 0 |
| 64 | 8 / 1.0 | 4.9 | 4 | 0 |

In every run the victim kept honest peers and received every message. With many attacker groups,
though, its outbound slots, the ones it chose itself, went mostly to the attacker. A run with no
honest outbound peer kept its honest peers through inbound connections from honest nodes, which
dial the victim's record. An attacker that also controlled the victim's inbound slots, from 8 or
more groups (4 each), could cut it off completely. Bootnodes and `trusted` peers are one defence
against that: they are redialed whenever they are disconnected, and never banned or refused.

Saved peers are the other, for the moment an attacker likes best: a restart, when a node without
memory starts over from bootnodes and peer exchange. The simulator's `restart` scenario measures
it: the same network, but the attackers stay dormant until the victim restarts at 60 s, then all
attack at once; the run lasts 240 s. 12 seeds per row:

| Attacker groups | Saved peers: honest outbound (mean) | Runs with no honest peer | Without: honest outbound (mean) | Runs with no honest peer | Gossip misses without |
| --- | --- | --- | --- | --- | --- |
| 2 | 8.0 of 8 | 0 | 3.2 of 4.5 | 0 (7 with no honest outbound) | 0 |
| 4 | 8.0 of 8 | 0 | 1.3 of 4.7 | 0 (10 with no honest outbound) | 0 |
| 8 | 8.0 of 8 | 0 | 6.0 of 8 | 3 | 234 |
| 16 | 7.4 of 8 | 0 | 3.2 of 8 | 3 | 234 |
| 64 | 6.9 of 8 | 0 | 2.1 of 8 | 4 | 312 |

With saved peers, no victim lost its honest peers or a message. Without them, a third of the
victims facing 8 or more attacker groups were cut off completely.

## What the application provides

- `Config`:
  - the network, static key and advertised address;
  - capabilities, peer limits, bootnodes and trusted peers;
  - gossip fanouts and maximum size;
  - request and notice kinds.
- `driver::start(config, listen)` returns a `Handle` and a receiver of `Event`s.
  - Answer every `Event::Gossip` with `Handle::validated`.
  - Answer every `Event::Request` with `Handle::respond`.
  - Report bad responses with `Handle::report`.
