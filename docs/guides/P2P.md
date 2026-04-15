# The Base P2P Networking Stack

This guide walks through the peer-to-peer networking architecture used by Base nodes. It is written
for someone who may not have much prior experience with P2P systems and wants to understand how the
different networking layers fit together, why there are two of them, and where to find the relevant
code.


## What is P2P networking and why does it matter?

At the heart of any blockchain is a network of computers that need to talk to each other without
relying on a single central server. Peer-to-peer networking is the mechanism that makes this
possible. Instead of every node connecting to one authority to get the latest blocks and
transactions, each node connects directly to a handful of other nodes, and those nodes connect to
others, forming a mesh (a web of overlapping connections where every node can reach every other node
through some path).

When one node learns about a new block or transaction, it tells its neighbors, who tell their
neighbors, and within seconds the entire network has seen it. This is called gossip, and it is the
fundamental way information propagates across a blockchain.

P2P networking handles three core problems. First, discovery: how does a node that just started up
find other nodes to connect to? Second, transport: once two nodes know about each other, how do they
establish a secure connection and exchange data? Third, gossip: once connected, how do they
efficiently broadcast new information to the entire network without flooding it?


## Why Ethereum has two P2P layers

Before Ethereum's transition to proof-of-stake (an event known as The Merge in September 2022),
there was a single P2P network. Every node ran one piece of software, an execution client like Geth,
that handled everything: discovering peers, gossiping transactions, propagating newly mined blocks,
and reaching consensus under proof-of-work (where nodes compete to solve computational puzzles to
produce blocks). The networking stack for all of this was DevP2P, a set of protocols purpose-built
for Ethereum starting in 2014.

The Merge introduced a second piece of software: the consensus client. This client is responsible
for following the beacon chain (a separate chain introduced to coordinate proof-of-stake consensus,
where nodes lock up cryptocurrency as collateral and are selected to produce blocks proportionally),
and deciding which blocks are canonical. The Beacon Chain had been designed from scratch with a
completely different networking stack, libp2p rather than DevP2P, because libp2p is more modular and
better suited to the publish-subscribe messaging patterns (where nodes subscribe to named channels
called "topics" and receive all messages published to those topics) needed for attestation
(validator votes that a particular block is valid) and block gossip in a proof-of-stake system.
Merging these two protocol stacks into one would have been an enormous engineering effort with
little practical benefit, so the pragmatic decision was to keep them separate and bridge them with a
local API.

So after The Merge, every Ethereum node runs two separate processes with two separate P2P networks.
The execution layer (EL) still uses DevP2P with RLPx (its encrypted transport layer, described in
detail later) to exchange transactions and sync historical block data with other EL nodes. The
consensus layer (CL) uses libp2p with gossipsub (its message broadcasting protocol, also described
later) to propagate beacon blocks and attestations. The two layers talk to each other locally
through the Engine API, which is just a JSON-RPC connection over HTTP or WebSocket on localhost.

For a rollup like Base (a type of blockchain that executes transactions on its own chain but posts
the transaction data back to Ethereum for security), this same two-layer architecture applies, but
with important differences in what gets gossiped. On Ethereum L1 (Layer 1, the base Ethereum chain),
the consensus layer gossips beacon blocks and attestations from hundreds of thousands of validators.
On Base (an L2, or Layer 2, chain that runs on top of Ethereum), the consensus layer gossips L2
execution payloads, which are the new blocks produced by the sequencer (the single designated node
that orders and produces L2 blocks).

The sequencer signs each block with its private key so that other nodes can verify the block
genuinely came from the authorized block producer, rather than from an attacker injecting fake
blocks. It then publishes the signed block to the CL gossip network, and every other Base node picks
it up from there.

This is a performance optimization, not a security mechanism. The rollup's security comes entirely
from L1: batch data posted to Ethereum L1 is the canonical source of truth, and the derivation
pipeline (software that reconstructs the L2 chain state by reading batch data from L1) can
reconstruct the entire L2 chain from L1 data alone. P2P gossip merely allows nodes to learn about
new L2 blocks a few minutes before the corresponding batch data appears on L1, reducing latency for
users. The EL side still runs a DevP2P network for transaction pool gossip and historical sync,
though in practice the sequencer often receives transactions directly rather than through EL gossip.


## How the CL and EL communicate: the Engine API

Before diving into each P2P stack individually, it helps to understand how they connect. The
consensus client and the execution client do not share a P2P network. They share a local RPC bridge
called the Engine API, a JSON-RPC interface exposed by the execution client (typically on port 8551)
and secured with JWT (JSON Web Token) authentication, a shared-secret mechanism that proves the
caller is authorized. This ensures that only the co-located consensus client can issue these
sensitive calls.

The Engine API has three core methods. Think of it as a three-step conversation: "start building a
block," "give me what you built," and "here's a block someone else built, check it."

`engine_forkchoiceUpdated` tells the execution client which block is the current head, which is the
latest safe block, and which is the latest finalized block. When the consensus client also passes
optional "payload attributes" (a timestamp, fee recipient, and other parameters describing the block
to build), this call additionally instructs the execution client to begin assembling a new block.
`engine_getPayload` retrieves the block that the execution client has been assembling from its
mempool (the pool of pending transactions waiting to be included in a block). `engine_newPayload`
sends an execution payload received from gossip to the execution client for validation and
execution. The execution client re-executes all transactions, verifies the state root (a
cryptographic fingerprint of the entire blockchain state after executing the block's transactions),
and responds with VALID, INVALID, or SYNCING (meaning the node hasn't caught up to the chain tip yet
and can't validate the block).

For block production on Base, the flow is: the sequencer's consensus client calls
`engine_forkchoiceUpdated` with payload attributes to start building, waits, calls
`engine_getPayload` to retrieve the built payload, signs it, and publishes it to the CL gossip
network. For block validation, the flow is reversed: the consensus client receives a signed block
from gossip, validates the signature, sends the execution payload to the execution client via
`engine_newPayload`, and based on the validity response, decides whether to accept the block.

This separation means each layer can evolve its networking independently. It also means you can swap
out implementations. As long as your CL and EL speak the Engine API, they work together.


## The Consensus Layer P2P Stack

The Base consensus P2P stack lives under
[`crates/consensus/`](https://github.com/base/base/tree/main/crates/consensus) and is composed of
four main crates that layer on top of each other: peers, disc, gossip, and the network actor in the
service crate. Let's walk through each one from the bottom up.


### Peers: the foundation

The [`base-consensus-peers`](https://github.com/base/base/tree/main/crates/consensus/peers) crate
provides the fundamental types for identifying and managing peers on the consensus network.

The most important concept here is the ENR, which stands for Ethereum Node Record (defined in
[EIP-778](https://eips.ethereum.org/EIPS/eip-778)). An ENR is a compact, self-describing identity
document that a node uses to advertise itself to the network. It is encoded using RLP (Recursive
Length Prefix, Ethereum's standard binary serialization format) and contains a cryptographic
signature, a sequence number that only ever goes up (so newer records always have higher numbers),
and a set of sorted key-value pairs. The predefined keys include `id` (the identity scheme,
currently "v4" for secp256k1, the specific elliptic curve used for Ethereum's public-key
cryptography), `secp256k1` (the node's compressed 33-byte public key), `ip` (IPv4 address), and
`tcp`/`udp` (port numbers). When a node's information changes (say, its IP address rotates), it
increments the sequence number and re-signs the record, and the signature ensures the record cannot
be tampered with. ENRs are deliberately kept small (maximum 300 bytes) because they are relayed
frequently through the discovery protocol and may need to fit in constrained transports like DNS TXT
records (a method for distributing node records through the domain name system).

Different layers of the Ethereum stack use ENR extension keys for chain-specific metadata. On
Ethereum L1's execution layer, ENRs carry an `eth` key with fork ID information. On the L1 consensus
layer, they carry an `eth2` key with the fork digest and attestation subnet bitfield. On Base (and
OP Stack chains more broadly), every ENR includes an `opstack` key that encodes the L2 chain ID and
a version number. This is how nodes on different chains (say, Base Mainnet vs Base Sepolia) can tell
each other apart during discovery. The textual representation of an ENR is a base64-encoded string
prefixed with `enr:`, which you will see in configuration files and bootnode lists.

The [`BaseEnr`](https://github.com/base/base/blob/main/crates/consensus/peers/src/enr.rs) struct
handles this encoding:

```rust
/// The unique L2 network identifier
pub struct BaseEnr {
    /// Chain ID
    pub chain_id: u64,
    /// The version. Always set to 0.
    pub version: u64,
}

impl BaseEnr {
    /// The ENR key literal string for the consensus layer.
    pub const OP_CL_KEY: &str = "opstack";

    /// Constructs a BaseEnr from a chain id.
    pub const fn from_chain_id(chain_id: u64) -> Self {
        Self { chain_id, version: 0 }
    }
}
```

When a node discovers another node's ENR, it validates it using
[`EnrValidation`](https://github.com/base/base/blob/main/crates/consensus/peers/src/enr.rs). The
validation checks that the `opstack` key is present, that it decodes correctly, and that the chain
ID matches. If a node on Base Mainnet (chain ID 8453) encounters an ENR with a different chain ID,
it simply ignores it.

The peers crate also provides a
[`BootStore`](https://github.com/base/base/blob/main/crates/consensus/peers/src/store.rs), which is
a simple JSON file that persists discovered ENRs to disk. This way, when a node restarts, it doesn't
have to start discovery from scratch. The boot store caps out at 2048 entries and prunes the oldest
ones when full.


### Discovery: finding peers with discv5

The [`base-consensus-disc`](https://github.com/base/base/tree/main/crates/consensus/disc) crate
implements peer discovery using the discv5 protocol. Discv5 is a UDP-based protocol that maintains a
distributed hash table (DHT) of node records. It is the successor to discv4 (used by the EL) and was
designed specifically for the consensus layer's needs.

Discv5 is inspired by Kademlia, an academic distributed hash table (DHT) protocol from 2002. A DHT
is a system where many computers collectively maintain a lookup directory without any central
coordinator. The central insight of Kademlia is using XOR as a distance metric between node
identifiers. Every node has a 256-bit ID derived from its public key, and the "distance" between any
two nodes is computed as the XOR of their IDs, interpreted as a number. This distance is symmetric
(the distance from A to B equals the distance from B to A) and has nothing to do with physical or
network distance. Two nodes with similar IDs might be on opposite sides of the planet. Think of it
like comparing two phone numbers digit by digit: two numbers that share a long common prefix are
"close" in Kademlia space, even though the people holding those numbers might live in different
countries.

Each node maintains a routing table organized into 256 "k-buckets," one for each possible bit-length
of the XOR distance. Bucket 0 contains nodes whose IDs differ from the local node's ID only in the
least significant bit, and bucket 255 contains nodes that differ in the most significant bit. Each
bucket holds up to k entries (typically k=16). Imagine organizing your contacts into folders: one
folder for people whose IDs differ from yours only in the last bit, another for those differing in
the second-to-last bit, and so on. You end up with more contacts in the "close" folders and fewer in
the "far" folders, which is exactly the right distribution for efficient lookups.

To discover new peers, a node performs an iterative lookup. It picks a target ID (often random, for
the purpose of populating its routing table), finds the closest nodes it already knows, and sends
them FINDNODE requests. Unlike discv4 which asks for nodes "near" a specific ID, discv5 FINDNODE
requests specify a logarithmic distance, which is a number from 0 to 256 representing the bit
position where IDs first differ (essentially asking "give me all nodes at XOR distance 2^n from
you"). This is more efficient and harder to abuse. The node receives lists of nodes at that
distance, queries those newly learned nodes, and gets progressively closer to the target with each
round. Through repeated lookups, a node builds a comprehensive view of the network.

New nodes bootstrap by connecting to a small set of well-known bootnodes whose addresses are
hardcoded into the client. The bootnode responds to FINDNODE requests, giving the new node its first
set of peers. From there, the new node performs several random lookups to fill its routing table,
and within minutes it has a healthy set of diverse peers.

The [`Discv5Driver`](https://github.com/base/base/blob/main/crates/consensus/disc/src/driver.rs)
orchestrates the discovery process. When it starts, it goes through a clear sequence. First, it
initializes the discv5 UDP service with exponential backoff retries (waiting progressively longer
between attempts — e.g. 1s, 2s, 4s, 8s). If that succeeds, it then starts the
event stream, retrying
up to 10 times with 2-second delays if the event stream fails to initialize. Then it bootstraps by
loading the boot store from disk and adding hardcoded boot nodes. Each ENR is validated against the
expected chain ID before being added to the routing table:

```rust
let validation = EnrValidation::validate(&enr, chain_id);
if validation.is_invalid() {
    trace!(target: "discovery::bootstrap", enr = ?enr, validation = ?validation,
        "Ignoring Invalid Bootnode ENR");
    continue;
}

if let Err(e) = disc.add_enr(enr.clone()) {
    debug!(target: "discovery::bootstrap", error = ?e, "Failed to add enr");
    continue;
}

store.add_enr(enr);
```

After bootstrapping, the driver enters its main loop where it does several things concurrently using
`tokio::select!` (a Rust macro that waits on multiple asynchronous operations simultaneously and
runs the code for whichever one completes first). It runs periodic random node queries (every 5
seconds by default) to continually discover new peers. It listens for discv5 events like
`Discovered`, `SessionEstablished`, and `UnverifiableEnr`, and when it sees a valid ENR, it forwards
it through an `mpsc` (multi-producer, single-consumer) channel to the gossip layer so it can dial
the peer. An `mpsc` channel is a thread-safe queue where multiple senders can push messages to a
single receiver — it is the primary way components communicate in async Rust. The driver also
persists the current set of known ENRs to the boot store every 60 seconds.

The driver communicates with the rest of the system through a
[`Discv5Handler`](https://github.com/base/base/blob/main/crates/consensus/disc/src/handler.rs),
which is just a thin wrapper around an `mpsc::Sender`. Other parts of the system can request
metrics, peer lists, the local ENR, or ask the discovery service to ban specific addresses. This
channel-based design avoids the need for shared mutable state across async boundaries.


### Gossip: broadcasting blocks with libp2p and gossipsub

The [`base-consensus-gossip`](https://github.com/base/base/tree/main/crates/consensus/gossip) crate
is where the real action happens. This is the layer that actually receives and broadcasts L2 blocks
across the network.

Gossipsub is a pub/sub (publish/subscribe) protocol built on libp2p that solves the problem of
efficiently distributing messages in a decentralized network where no single node can be trusted.
The naive approach, called floodsub, would forward every message to every peer, which works but
consumes enormous bandwidth because every node receives every message from multiple sources.
Gossipsub constrains this by building a sparse "mesh" overlay for each topic.

When a node subscribes to a topic, it tells its connected peers via a subscription message. It then
selects D peers (the "desired mesh degree") that are also subscribed to the same topic and
establishes mesh links with them by sending GRAFT control messages. These mesh links are symmetric:
if node A grafts node B, node B also considers A part of its mesh. When a message is published, it
flows eagerly through these mesh links. A node that receives a message forwards it to all its mesh
peers for that topic (except the one it received it from). This creates a connected overlay where
messages propagate in O(log N) hops with bounded bandwidth per node.

The gossip layer adds reliability on top of the mesh. Every heartbeat interval, each node sends
IHAVE control messages to a random subset of non-mesh peers, listing the IDs of messages it has
recently seen. If a peer has not received one of those messages (perhaps because the mesh path
failed or was slow), it replies with IWANT, and the original node sends the full message. This lazy
repair mechanism ensures that even if the mesh is temporarily partitioned or a peer goes offline,
messages still reach everyone within a few heartbeat rounds. PRUNE messages are the counterpart to
GRAFT: a node sends PRUNE when it wants to remove a peer from its mesh, either because the mesh is
too large or because the peer has a poor score. Pruned peers are not disconnected, they simply move
from the eager-push mesh to the lazy-gossip pool.

Gossipsub v1.1, which Base uses, added a peer scoring system where each node evaluates its peers
based on their behavior (whether they deliver valid messages promptly, whether they send duplicates
or spam) and preferentially keeps well-scored peers in the mesh while pruning poorly scored ones.
For example, a peer that repeatedly sends invalid messages might accumulate a negative score, and
once it drops below a configurable threshold, it gets pruned from the mesh and eventually
disconnected. Gossipsub v1.1 also introduced flood publishing (where the original publisher sends to
all connected peers, not just mesh peers), though Base has this disabled by default to conserve
bandwidth.

The gossipsub configuration in Base is defined in
[`gossip/src/config.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/config.rs).
The key parameters are:

```rust
pub const DEFAULT_MESH_D: usize = 8;       // target peers in mesh
pub const DEFAULT_MESH_DLO: usize = 6;     // minimum before mesh repair
pub const DEFAULT_MESH_DHI: usize = 12;    // maximum before pruning
pub const DEFAULT_MESH_DLAZY: usize = 6;   // peers for lazy gossip
pub const GOSSIP_HEARTBEAT: Duration = Duration::from_millis(500);
pub const MAX_GOSSIP_SIZE: usize = 10 * (1 << 20);  // 10 MB
```

What these numbers mean in practice: each node tries to maintain connections to 8 other nodes in its
mesh for each topic. If it drops below 6, it will graft (add) new peers. If it goes above 12, it
will prune some. Every 500 milliseconds, the heartbeat fires and the protocol checks the mesh
health. The lazy gossip parameter means that for messages a node has seen but didn't forward via the
mesh, it will still tell 6 additional peers about those messages via lightweight metadata so they
can request them if needed.

Messages are compressed with Snappy (a fast compression algorithm prioritizing speed over
compression ratio) before being sent, and each message ID is computed by SHA-256 hashing (a
cryptographic hash function that produces a unique fixed-size fingerprint of arbitrary data) the
decompressed content, with a domain prefix (a few extra bytes prepended before hashing to
distinguish valid from invalid encodings). This is how the network deduplicates messages.

The gossip topics are where Base's L2-specific design becomes apparent. The
[`BlockHandler`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/handler.rs)
manages four versioned topics, each corresponding to a different protocol version:

```rust
blocks_v1_topic: IdentTopic::new(format!("/optimism/{chain_id}/0/blocks")),
blocks_v2_topic: IdentTopic::new(format!("/optimism/{chain_id}/1/blocks")),
blocks_v3_topic: IdentTopic::new(format!("/optimism/{chain_id}/2/blocks")),
blocks_v4_topic: IdentTopic::new(format!("/optimism/{chain_id}/3/blocks")),
```

For Base Mainnet (chain ID 8453), these resolve to `/optimism/8453/0/blocks` through
`/optimism/8453/3/blocks`. The version is selected based on which hardfork (a protocol upgrade that
changes the rules of the network, activated at a specific timestamp) is active at the block's
timestamp. V1 is for pre-Canyon blocks, V2 for Canyon/Delta, V3 for Ecotone, and V4 for Isthmus.
Each version uses a slightly different encoding for the execution payload envelope. When a node
subscribes to the gossip network, it subscribes to all four topics simultaneously so it can handle
blocks from any protocol version.

The `BlockHandler` implements the
[`Handler`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/handler.rs) trait,
which has two methods: `handle()` for processing incoming messages and `topics()` for declaring
which topics it cares about. When a gossip message arrives, the handler first checks which topic it
came from to determine the correct decoding version, then decodes the payload, and then validates
it:

```rust
fn handle(&mut self, msg: Message) -> (MessageAcceptance, Option<NetworkPayloadEnvelope>) {
    let decoded = if msg.topic == self.blocks_v1_topic.hash() {
        NetworkPayloadEnvelope::decode_v1(&msg.data)
    } else if msg.topic == self.blocks_v2_topic.hash() {
        NetworkPayloadEnvelope::decode_v2(&msg.data)
    } else if msg.topic == self.blocks_v3_topic.hash() {
        NetworkPayloadEnvelope::decode_v3(&msg.data)
    } else if msg.topic == self.blocks_v4_topic.hash() {
        NetworkPayloadEnvelope::decode_v4(&msg.data)
    } else {
        return (MessageAcceptance::Reject, None);
    };

    match decoded {
        Ok(envelope) => match self.block_valid(&envelope) {
            Ok(()) => (MessageAcceptance::Accept, Some(envelope)),
            Err(err) => (err.into(), None),
        },
        Err(err) => (MessageAcceptance::Reject, None),
    }
}
```

The result is a `MessageAcceptance` that feeds back into gossipsub's peer scoring system. `Accept`
means the message was valid and the peer gets credit. `Reject` means the message was invalid and the
peer's score takes a hit. `Ignore` is used for already-seen blocks, which don't penalize the peer.


### Block validation: how gossip keeps the network honest

The block validation logic in
[`block_validity.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/block_validity.rs)
is one of the most important pieces of the P2P stack because it determines what the node will accept
from the network. The validation performs several checks in sequence, and the order matters.

First, the timestamp must be within an acceptable window. The block's timestamp cannot be more than
5 seconds in the future or more than 60 seconds in the past. This prevents replay attacks (where an
attacker re-broadcasts old, legitimate messages to confuse the network) and rejects stale blocks.

Second, the block hash is recomputed from the payload contents and compared against the hash
included in the envelope. If they don't match, someone tampered with the payload.

Third, version-specific payload constraints are validated. For example, V3 (Ecotone) and later
payloads must have a non-empty parent beacon block root and zero blob gas usage. V4 (Isthmus)
payloads must include a withdrawals root. This step catches blocks that are structurally invalid for
their protocol version.

Fourth, the handler checks its seen hashes tracking. It maintains a `BTreeMap` keyed by block
height, with a cache size of 1,000 entries. If more than 5 different blocks have already been stored
for the same height (i.e., a 6th unique block is accepted but a 7th is rejected), the block is
rejected. If this exact block hash has been seen before, it is ignored (with no penalty to the
sending peer). You might wonder why there would be multiple valid blocks at the same height if there
is only one sequencer — this can happen when the sequencer produces a
replacement block, for example
after a reorg (chain reorganization) triggered from L1.

Fifth, and this is critical for rollup security, the signature is verified. The sequencer signs each
block with its private key, and every node knows the expected signer's address. The expected signer
is read first, then the signature is recovered from the payload hash using ECDSA recovery (a
property of elliptic curve signatures that lets you compute the signer's public key from just the
signature and the signed message). If the recovered address doesn't match the expected signer, the
block is rejected:

```rust
let msg = envelope.payload_hash.signature_message(self.rollup_config.l2_chain_id.id());
let block_signer = *self.signer_recv.borrow();

let Ok(msg_signer) = envelope.signature.recover_address_from_prehash(&msg) else {
    return Err(BlockInvalidError::Signature);
};

if msg_signer != block_signer {
    return Err(BlockInvalidError::Signer { expected: block_signer, received: msg_signer });
}
```

Only after the signature passes does the handler insert the block hash into the seen hashes map,
marking it as processed. This prevents spam without being so aggressive that it rejects legitimate
competing blocks.


### Connection gating: controlling who connects

The [`ConnectionGater`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/gater.rs)
is a rate-limiting layer that controls which peers can connect. It tracks dial attempts per peer
address and enforces a configurable dial period (default: 1 hour). By default, redialing is disabled
entirely — a peer can only be dialed once per period. The CLI overrides this to allow up to 500
redials per period via `--p2p.redial`. The gater also supports explicitly blocking peers by ID, IP
address, or subnet, and protecting specific peers from being disconnected regardless of their score.

This is important for network health. Without connection gating, a misbehaving or misconfigured node
could repeatedly attempt to connect, wasting resources. The gater ensures that connection attempts
are bounded and that known-bad actors can be blocked at the connection level rather than just at the
gossip level.


### The libp2p Behaviour: combining protocols

The [`Behaviour`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/behaviour.rs)
struct is a libp2p `NetworkBehaviour` that combines several sub-protocols into a single swarm
(libp2p's term for the combination of a transport layer, a set of protocol behaviors, and connection
management — essentially the "networking engine"):

```rust
#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub ping: libp2p::ping::Behaviour,
    pub gossipsub: libp2p::gossipsub::Behaviour,
    pub identify: libp2p::identify::Behaviour,
    pub sync_req_resp: libp2p_stream::Behaviour,
}
```

The `ping` behaviour sends periodic keepalive pings to connected peers and measures round-trip
times. The `gossipsub` behaviour handles the actual block gossip. The `identify` behaviour exchanges
capability information between peers when they first connect (the Base node advertises its agent
version as `"base"`). The `sync_req_resp` behaviour supports a legacy request-response protocol
called `payload_by_number` that is part of the OP Stack spec. This is being deprecated, and the Base
implementation responds with "not found" to all requests, but it is still present so that op-nodes
don't penalize Base nodes for not supporting it.

The `GossipDriver`
([`gossip/src/driver.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/driver.rs))
wraps the swarm and provides higher-level operations. Its `start()` method binds the swarm to a TCP
address (default `0.0.0.0:9222`), waits for the `NewListenAddr` event confirming
the listener is up,
and then returns. Its `publish()` method takes an execution payload envelope (the signed wrapper
around a block's contents — transactions, state root, gas used, etc.), selects
the appropriate topic
based on the block's timestamp and active hardfork, encodes it with version-appropriate
serialization, and publishes it to gossipsub. Its `dial()` method takes an ENR from discovery,
validates the chain ID, extracts the TCP multiaddr (a self-describing network address format used by
libp2p, e.g. `/ip4/192.168.1.1/tcp/9222`), checks the connection gate, and initiates a connection.


### Putting it all together: the Network Actor

The
[`NetworkActor`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/actor.rs)
in the service crate ties everything together. It follows the actor pattern, a concurrency design
where each component runs as an independent task that communicates with other components exclusively
through message channels, avoiding shared mutable state. It is the top-level component that the
consensus node's main loop interacts with. The actor is generic over a
[`GossipTransport`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/transport.rs)
trait, which allows swapping out the real networking stack for an in-process test transport:

```rust
#[async_trait]
pub trait GossipTransport: Send + 'static {
    type Error: std::fmt::Debug + Send + 'static;

    async fn publish(&mut self, payload: BaseExecutionPayloadEnvelope) -> Result<(), Self::Error>;
    async fn next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope>;
    fn set_block_signer(&mut self, address: Address);
    fn handle_p2p_rpc(&mut self, request: P2pRpcRequest);
}
```

The production implementation is
[`NetworkHandler`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/handler.rs),
which composes the `GossipDriver` and `Discv5Handler` together. It runs a `tokio::select!` loop that
simultaneously handles several things: receiving ENRs from discovery and dialing them as new gossip
peers, receiving blocks from gossip and forwarding them to the consensus engine, publishing blocks
produced locally (if this node is the sequencer), inspecting peer scores every 15 seconds and
banning low-scoring peers (disconnecting them from gossip and banning their addresses in discovery),
and handling administrative RPC requests.

The
[`NetworkDriver`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/driver.rs)
handles the startup sequence. It starts the gossip swarm first, gets back the actual listen address,
optionally updates the local ENR with that address (so that other nodes discover the correct port),
and then starts the discovery service. This ordering matters because the ENR needs to contain the
real TCP port that gossip is listening on.

The `NetworkActor` communicates with the rest of the consensus node through `mpsc` channels bundled
in a
[`NetworkInboundData`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/actor.rs)
struct:

```rust
pub struct NetworkInboundData {
    pub signer: mpsc::Sender<Address>,
    pub p2p_rpc: mpsc::Sender<P2pRpcRequest>,
    pub admin_rpc: mpsc::Sender<NetworkAdminQuery>,
    pub gossip_payload_tx: mpsc::Sender<BaseExecutionPayloadEnvelope>,
}
```

Other actors in the node (like the sequencer or the admin RPC server) use these senders to push data
to the network actor. The `signer` channel is used to update the expected unsafe block signer
address. The `gossip_payload_tx` channel is used by the sequencer to publish newly produced blocks.
This message-passing architecture keeps all networking concerns isolated in one actor without shared
mutable state.


### CL startup flow from the command line

When you launch the Base consensus binary, the P2P configuration comes from CLI flags defined in
[`base-client-cli`](https://github.com/base/base/tree/main/crates/client/cli). The key flags include
`--p2p.listen.tcp` (default 9222) and `--p2p.listen.udp` (default 9223) for the local bind
addresses, `--p2p.advertise.ip` for NAT (Network Address Translation) scenarios where the node is
behind a router and its public IP address differs from its local IP, `--p2p.priv.path` for the
node's secp256k1 private key, and the mesh parameters like `--p2p.gossip.mesh.d` for the target mesh
size.

The startup flow in the CLI's `exec()` function loads the rollup configuration, parses the P2P
arguments into a `NetworkConfig`, constructs a `NetworkBuilder` with both the gossip and discovery
builders, and passes it to the `RollupNodeBuilder` which starts the `NetworkActor`. From that point
on, the node is live on the consensus P2P network.


## The Execution Layer P2P Stack

The execution layer P2P stack is built on reth, which is a high-performance Ethereum execution
client written in Rust. The Base-specific customizations live under
[`crates/execution/`](https://github.com/base/base/tree/main/crates/execution), and the node
definition is in
[`crates/execution/node/`](https://github.com/base/base/tree/main/crates/execution/node).


### How reth handles networking

Reth implements the standard Ethereum execution layer networking stack. At the transport level, it
uses DevP2P with RLPx, a cryptographic transport protocol where peers perform a handshake to
establish an encrypted, authenticated session. The mechanics are similar in spirit to how HTTPS
secures web traffic: both nodes exchange temporary keys and derive a shared secret, after which all
communication is encrypted. (For the curious, the handshake uses ECIES, or Elliptic Curve Integrated
Encryption Scheme, with ephemeral key shares and Diffie-Hellman key agreement. The resulting session
is encrypted with AES-256 and authenticated with keccak256-based MACs. Understanding these
cryptographic details is not necessary for working with the codebase.) Once the encrypted channel is
up, the two nodes exchange "Hello" messages that negotiate which sub-protocols they both support.

On top of this transport, reth speaks the `eth/68` wire protocol (version 68 of the Ethereum
sub-protocol for exchanging chain data between execution clients). This handles block headers, block
bodies, receipts, and transaction announcements. For transaction gossip specifically, eth/68 uses a
two-phase announcement system: when a node receives a new transaction, it sends the full transaction
to a small random fraction of its peers and sends lightweight hash announcements (with type and size
metadata) to all other peers, who can then request the full transaction if they don't already have
it. Reth also supports the `snap/1` protocol for snapshot-based state synchronization, which allows
new nodes to download state much faster than replaying the entire chain history.

For peer discovery, reth supports both discv4 (the older UDP-based Kademlia protocol) and discv5
(the newer protocol also used by the consensus layer). Nodes can also be configured with static boot
nodes and DNS-based discovery.


### The BaseNetworkBuilder

The [`BaseNetworkBuilder`](https://github.com/base/base/blob/main/crates/execution/node/src/node.rs)
is the component that configures reth's network for Base. It has two configuration knobs:

```rust
pub struct BaseNetworkBuilder {
    pub disable_txpool_gossip: bool,
    pub disable_discovery_v4: bool,
}
```

The `disable_txpool_gossip` flag is particularly important for rollup nodes. When a node is
configured with a sequencer endpoint (meaning it should forward transactions to the sequencer rather
than including them in blocks itself), transaction pool gossip is disabled. This prevents the node
from broadcasting pending transactions to the rest of the network, because in a rollup the sequencer
is the only entity that orders transactions.

The `network_config` method assembles reth's `NetworkConfig` by applying the discovery settings. The
following code builds the configuration step by step: it sets the RLPx socket address, conditionally
disables discv4 discovery, enables discv5 discovery with boot nodes, and finally sets the
transaction gossip flag:

```rust
let network_builder = ctx
    .network_config_builder()?
    .apply(|mut builder| {
        let rlpx_socket = (args.addr, args.port).into();
        if disable_discovery_v4 || args.discovery.disable_discovery {
            builder = builder.disable_discv4_discovery();
        }
        if !args.discovery.disable_discovery {
            builder = builder.discovery_v5(
                args.discovery.discovery_v5_builder(
                    rlpx_socket,
                    ctx.config()
                        .network
                        .resolved_bootnodes()
                        .or_else(|| ctx.chain_spec().bootnodes())
                        .unwrap_or_default(),
                ),
            );
        }
        builder
    });

let mut network_config = ctx.build_network_config(network_builder);
network_config.tx_gossip_disabled = disable_txpool_gossip;
```

Notice that discv4 is disabled by default for Base (`--rollup.discovery.v4` defaults to false) while
discv5 is enabled. The boot nodes are resolved from either CLI arguments or the chain specification.
Once the config is built, the `build_network` method creates a `NetworkManager`, starts it, and logs
the local enode record (the DevP2P equivalent of an ENR — a URL-formatted node identifier like
`enode://<pubkey>@<ip>:<port>`).


### Transaction pool and gossip

The Base transaction pool is defined in
[`crates/execution/txpool/`](https://github.com/base/base/tree/main/crates/execution/txpool). It
extends reth's standard transaction pool with rollup-specific validation and ordering.

The
[`OpTransactionValidator`](https://github.com/base/base/blob/main/crates/execution/txpool/src/validator.rs)
wraps reth's `EthTransactionValidator` and adds L1 data gas fee checks. Every transaction on Base
incurs both an L2 execution gas cost and an L1 data fee (the cost of posting the transaction data to
Ethereum L1). The validator ensures that the sender's balance covers both fees. It also rejects
EIP-4844 blob transactions (a special transaction type used on L1 to carry large data blobs for
rollups, which are not meaningful on the L2 itself).

The ordering strategy is configurable via `--rollup.txpool-ordering` and defined in
[`ordering.rs`](https://github.com/base/base/blob/main/crates/execution/txpool/src/ordering.rs):

```rust
pub enum BaseOrdering<T> {
    CoinbaseTip(CoinbaseTipOrdering<T>),
    Timestamp(TimestampOrdering<T>),
}
```

`CoinbaseTip` is the standard Ethereum ordering where transactions with higher priority fees get
included first. `Timestamp` is a rollup-specific FIFO ordering where transactions are prioritized by
arrival time regardless of fee. The timestamp ordering can be useful for fairer transaction
sequencing.


### Transaction forwarding

For non-sequencer nodes, transactions received in the mempool need to be forwarded to the sequencer
for inclusion. This is handled by the consumer/forwarder pipeline in the txpool crate.

The
[`SpawnedConsumer`](https://github.com/base/base/blob/main/crates/execution/txpool/src/consumer/mod.rs)
polls the transaction pool for new pending transactions and broadcasts them through a
`tokio::broadcast` channel. The
[`SpawnedForwarder`](https://github.com/base/base/blob/main/crates/execution/txpool/src/forwarder/mod.rs)
subscribes to this broadcast channel and forwards each transaction via a custom JSON-RPC method
(`base_insertValidatedTransactions`) to configured builder endpoints. One forwarder task is spawned
per builder URL, so multiple downstream builders can receive transactions simultaneously. This
pipeline runs as background tasks on the node's task executor.


### EL P2P configuration flags

The execution layer P2P is configured through reth's standard network flags plus Base-specific
rollup flags defined in
[`args.rs`](https://github.com/base/base/blob/main/crates/execution/node/src/args.rs). The key flags
are:

`--rollup.sequencer` sets the sequencer endpoint for transaction forwarding.
`--rollup.disable-tx-pool-gossip` disables transaction gossip on the DevP2P network (this is a
separate flag — setting the sequencer endpoint does not automatically disable gossip, so operators
typically set both). `--rollup.discovery.v4` enables the legacy discv4 discovery protocol (disabled
by default since Base uses discv5). `--rollup.txpool-ordering` selects between `coinbase-tip` and
`timestamp` ordering strategies.

The standard reth network flags still apply: `--network.addr` and `--network.port` control the RLPx
bind address (default port 30303), `--network.discovery.disable-discovery` turns off all peer
discovery, and `--network.discovery.bootnodes` provides custom boot node addresses.


## How the two stacks interact at runtime

When a Base node is running, the consensus and execution processes work in tandem but network
independently. Here is the flow for receiving a new block:

The sequencer produces a new L2 block and signs it. The signed block is published as a
`NetworkPayloadEnvelope` to the CL gossip network on the appropriate versioned topic (for example,
`/optimism/8453/3/blocks` for Isthmus). Every consensus node's `BlockHandler` receives the message,
validates it (timestamp, hash, signature, deduplication), and if valid, marks it as `Accept` and
passes it to the consensus engine. The engine sends the execution payload to the local execution
client via `engine_newPayloadV*`. The execution client validates the block against the EVM, computes
the new state root, and reports the result back.

For transaction submission, the flow goes the other direction. A user submits a transaction to an EL
node's RPC endpoint. If transaction pool gossip is enabled, the transaction gets broadcast to other
EL peers via the DevP2P `eth/68` protocol. If a sequencer endpoint is configured, the transaction
forwarder sends it directly to the sequencer via JSON-RPC. The sequencer includes it in the next
block, which then propagates through the CL gossip network as described above.

Discovery on each layer is independent. The CL uses discv5 on a UDP port (default 9223) to find
other CL peers, validating ENRs by chain ID to ensure it only connects to Base nodes. The EL uses
discv5 (and optionally discv4) on its own UDP port to find other EL peers. The two discovery
networks are completely separate and serve different purposes.


## Summary of key files

**Consensus layer peers and ENR management:**

-
  [`crates/consensus/peers/src/enr.rs`](https://github.com/base/base/blob/main/crates/consensus/peers/src/enr.rs)
  — BaseEnr encoding and validation
-
  [`crates/consensus/peers/src/store.rs`](https://github.com/base/base/blob/main/crates/consensus/peers/src/store.rs)
  — BootStore persistence

**Consensus layer discovery:**

-
  [`crates/consensus/disc/src/driver.rs`](https://github.com/base/base/blob/main/crates/consensus/disc/src/driver.rs)
  — Discv5Driver event loop and bootstrap

**Consensus layer gossip:**

-
  [`crates/consensus/gossip/src/config.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/config.rs)
  — Gossipsub constants and configuration
-
  [`crates/consensus/gossip/src/handler.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/handler.rs)
  — BlockHandler and topic management
-
  [`crates/consensus/gossip/src/block_validity.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/block_validity.rs)
  — Block validation rules
-
  [`crates/consensus/gossip/src/behaviour.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/behaviour.rs)
  — libp2p Behaviour composition
-
  [`crates/consensus/gossip/src/gater.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/gater.rs)
  — Connection rate limiting
-
  [`crates/consensus/gossip/src/driver.rs`](https://github.com/base/base/blob/main/crates/consensus/gossip/src/driver.rs)
  — GossipDriver swarm management

**Consensus layer orchestration:**

-
  [`crates/consensus/service/src/actors/network/actor.rs`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/actor.rs)
  — NetworkActor definition
-
  [`crates/consensus/service/src/actors/network/handler.rs`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/handler.rs)
  — Production NetworkHandler transport
-
  [`crates/consensus/service/src/actors/network/driver.rs`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/driver.rs)
  — Network startup sequence
-
  [`crates/consensus/service/src/actors/network/transport.rs`](https://github.com/base/base/blob/main/crates/consensus/service/src/actors/network/transport.rs)
  — GossipTransport trait

**Execution layer node and networking:**

-
  [`crates/execution/node/src/node.rs`](https://github.com/base/base/blob/main/crates/execution/node/src/node.rs)
  — BaseNetworkBuilder and network configuration
-
  [`crates/execution/node/src/args.rs`](https://github.com/base/base/blob/main/crates/execution/node/src/args.rs)
  — Rollup-specific CLI arguments

**Execution layer transaction pool:**

-
  [`crates/execution/txpool/src/validator.rs`](https://github.com/base/base/blob/main/crates/execution/txpool/src/validator.rs)
  — OpTransactionValidator with L1 data gas checks
-
  [`crates/execution/txpool/src/ordering.rs`](https://github.com/base/base/blob/main/crates/execution/txpool/src/ordering.rs)
  — BaseOrdering (fee-based vs FIFO)
-
  [`crates/execution/txpool/src/consumer/`](https://github.com/base/base/tree/main/crates/execution/txpool/src/consumer)
  — Transaction pool consumer
-
  [`crates/execution/txpool/src/forwarder/`](https://github.com/base/base/tree/main/crates/execution/txpool/src/forwarder)
  — Transaction forwarder to sequencer


## Glossary

**Attestation** — A validator's vote that a particular block is valid.

**Beacon chain** — The chain introduced with Ethereum's proof-of-stake upgrade that coordinates
consensus.

**Bootnode** — A well-known node whose address is hardcoded into client
software, used to bootstrap new nodes into the network.

**CL (Consensus Layer)** — The part of an Ethereum node responsible for consensus (deciding which
blocks are canonical).

**DevP2P** — The execution layer's P2P protocol suite, including RLPx transport and the eth wire
protocol.

**DHT (Distributed Hash Table)** — A system where many computers collectively maintain a lookup
directory without any central coordinator.

**Discv4 / Discv5** — Node Discovery Protocol versions 4 and 5. UDP-based protocols for finding
peers.

**EL (Execution Layer)** — The part of an Ethereum node responsible for executing transactions and
maintaining state.

**Engine API** — The JSON-RPC interface that the consensus and execution layers use to communicate
locally.

**ENR (Ethereum Node Record)** — A signed, self-describing identity document that a node uses to
advertise itself (IP, ports, public key, chain metadata).

**EVM (Ethereum Virtual Machine)** — The runtime environment that executes
smart contract bytecode.

**Gossipsub** — A publish/subscribe protocol built on libp2p that uses a
mesh overlay for efficient message distribution.

**GRAFT / PRUNE** — Gossipsub control messages for adding or removing a peer from the mesh.

**Hardfork** — A protocol upgrade that changes the rules of the network, activated at a specific
timestamp.

**IHAVE / IWANT** — Gossipsub control messages for the lazy repair mechanism (advertising and
requesting missed messages).

**k-bucket** — A fixed-size list of peers at a particular XOR distance
in a Kademlia routing table.

**L1 (Layer 1)** — The base Ethereum chain.

**L2 (Layer 2)** — A chain that runs on top of L1 for scalability (e.g. Base).

**libp2p** — A modular networking framework used by the consensus layer, originally developed for
IPFS.

**Mempool** — The pool of pending transactions waiting to be included in a block.

**mpsc** — Multi-producer, single-consumer channel. A thread-safe queue
used for async communication in Rust.

**Multiaddr** — A self-describing network address format used by libp2p (e.g.
`/ip4/192.168.1.1/tcp/9222`).

**NAT (Network Address Translation)** — When a node is behind a router and its public IP differs
from its local IP.

**Reorg** — A chain reorganization where the network switches to a different sequence of blocks.

**RLP (Recursive Length Prefix)** — Ethereum's standard binary serialization format.

**RLPx** — The execution layer's encrypted TCP transport protocol.

**Rollup** — A type of blockchain that executes transactions on its own
chain but posts data back to Ethereum for security.

**Sequencer** — The single designated node that orders and produces L2 blocks.

**Snappy** — A fast compression algorithm used for gossip message compression.

**State root** — A cryptographic fingerprint of the entire blockchain state after executing a
block's transactions.

**Swarm** — libp2p's term for the combination of transport, protocol behaviors, and connection
management.

**XOR distance** — The distance metric used in Kademlia, computed by XOR-ing two node IDs. Has
nothing to do with geographic distance.
