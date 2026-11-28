# `base-tiger-p2p-fuzz`

Differential and stateful fuzzing of Base peer-to-peer networking: tiger's devp2p stack (`RLPx`,
ECIES, discv5, eth) against base-reth and Nethermind, and tp2p (tiger's new p2p layer). Findings are classified as
spec-required, harmless or exploitable, and go to the coordinator, never upstream or public.

## tp2p

```sh
base-tiger-p2p-fuzz tp2p wire [--seed S] [--iters N]       # message decoding and chunk reassembly
base-tiger-p2p-fuzz tp2p handshake [--seed S] [--iters N]  # Noise XK acts and transport frames
base-tiger-p2p-fuzz tp2p live [--seed S]                   # attacking peers against a live node
```

- `wire` mutates valid encodings of every message (bit flips, truncation, huge count fields,
  insertions) and chunk streams (reordered, duplicated, huge declared lengths). A decoder must
  not panic and must refuse anything it cannot re-encode byte for byte.
- `handshake` tampers with one of Act One, Two, Three or a transport frame per case; every
  tampered input must be refused and every untampered one accepted.
- `live` starts a tp2p node per attack, runs the attack from its own loopback address, then
  checks that an honest peer still completes a handshake and a ping. Attacks: slow-loris
  handshake, a flood of pending handshakes from one IP, a garbage Act One, a message before
  Status, a 4 `GiB` declared length, a chunk without a start, a partial message kept alive with
  pings, an `IWant` over the id limit, and a request flood over the rate limit. It also reports
  the process's resident memory.

## Simulation

```sh
base-tiger-p2p-fuzz sim --scenario <gossip|partition|eclipse|flood|silent|requests|equivocate|all> \
  [--seed S] [--seeds N] [--nodes N] [--attackers N] [--attacker-groups G] [--fanout F] [--verbose]
```

A deterministic discrete-event simulation of tp2p's sans-IO core (`base_tiger_p2p::Node`): a
seeded scheduler, a per-node uplink, FIFO links, partitions, drops and delays, and malicious
nodes (flooding, silent, equivocating, eclipsing with poisoned peer exchange). Every run checks
safety (no honest node accepts an invalid message), liveness (every honest node receives every
accepted message once the network heals), eclipse (a victim keeps at least one honest peer),
request answers, and that queues stay within their caps.

## tp2p devnet acceptance

`devnet/` runs anvil as L1 and three base-consensus and tiger pairs, all built with `--features
tp2p` and running with libp2p and devp2p off: a sequencer, a follower (EL sync and unsafe blocks
over tp2p) and a stateless tiger that gets its witnesses from tiger peers. The stateless tiger
reads its start block from the sequencer's RPC through a proxy that logs every method it calls.
The scripts run each process as an Amp orb service.

```sh
devnet/down.sh; devnet/up.sh    # data and logs in /tmp/tp2p-devnet
devnet/check.sh                 # same hash and state root on every block, open sockets, upstream calls
```

## EL differential

`el` compares tiger's devp2p stack (`base-tiger-net`) against base-reth (the reth crates pinned in
the root `Cargo.toml`, plus the `base-reth-node` binary) and Nethermind (the Base fork,
github.com/meyer9/nethermind `base-mainnet`). Everything is deterministic from `--seed`.

### In-process decoding

```sh
cargo run --release -p base-tiger-p2p-fuzz -- el decode --seed 1 --iters 20000 \
  [--target snappy|p2p|eth|status|forkid|ecies|enr|discv5] [--out FILE.json]
```

Each target generates valid inputs and mutates three of every four: truncation, length-prefix
edits, oversize, non-canonical RLP (leading zeros, a single byte wrapped as a string, long-form
short lengths), list/string swaps, trailing bytes, extra list elements, unknown message ids. Both
sides decode the same bytes, and the run compares accept or reject and the normalised decoded
values. Disagreements are grouped by class; each group keeps a minimized input.

| target | tiger | base-reth |
|---|---|---|
| `snappy` | `snappy_decompress` / `snappy_compress` | `snap` raw, with the `RLPx` 16 `MiB` declared-size check |
| `p2p` | Hello, Disconnect, Ping, Pong, `negotiate` | `reth-eth-wire` `P2PMessage`, `HelloMessage`, shared capabilities |
| `eth` | eth/68 and eth/69 messages as `Peer` and `Downloader` read them (`split_request_id`, `HeadersRequest`, `SealedHeader`, `Vec<BlockBody>` decoding) | `ProtocolMessage::decode_message` |
| `status` | Status decoding and accept rules | `reth-eth-wire` handshake checks |
| `forkid` | `ForkSchedule` compute and validate | `alloy-eip2124` |
| `ecies` | `ecies_accept`, `handshake_packet_len`, `FrameCodec` | `reth-ecies` `ECIES` auth, ack, header and MAC |
| `enr` | `Enr` decode and verify | `enr` crate (k256) |
| `discv5` | a live tiger `Discv5` service on loopback (its packet decoder is private) | sigp `Packet::decode`, `basev0` identity |

The harness's own ECIES initiator and frame codec (`src/el/wire.rs`) are written from the
devp2p spec, independently of both sides.

### Live

Nodes run on one devnet: a Regolith genesis (chain 845300) shared by all three, with blocks
built over the Engine API on base-reth and imported by the others, so every node holds the same
24 blocks (20 of them about 128 `KiB`, for the soft response limit).

```sh
el live setup --dir /tmp/el-live          # genesis.json, jwt.hex, node keys
# start the nodes as services (amp orb service start ...), then:
el live chain --dir /tmp/el-live --node reth=AUTH,HTTP --node tiger=AUTH,HTTP --blocks 24 --big 20
el live chain --dir /tmp/el-live --replay --node reth=AUTH,HTTP --node nethermind=AUTH,HTTP
el live rlpx --target reth=enode://ID@127.0.0.1:30311 --target tiger=enode://ID@... \
  --target nethermind=enode://ID@... [--scenario SUBSTRING] [--seed S] [--out FILE.json]
el live discv5 --target reth=enode://ID@127.0.0.1:UDP ... [--seed S] [--out FILE.json]
```

Nethermind runs from a `dotnet publish` of the fork in the .NET SDK image, with a Parity chainspec
made from Base mainnet's (forks after Regolith removed, genesis and alloc from `genesis.json`).
It imports the same blocks (`--replay`) and answers discv5, but it currently disconnects the
scripted `RLPx` peer with `UselessPeer` right after Hello, so its `rlpx` results are not yet usable.

`rlpx` is a scripted `RLPx` peer that sets every byte: the handshake, frame headers, compression
and message ids. About 105 scenarios cover Hello and capability negotiation; Status and fork id
(other genesis, past and future fork ids, network id, version, Status twice, a message before
Status); `GetBlockHeaders` (skip, reverse, count 0, over-limit counts, unknown starts, u64
overflow); `GetBlockBodies` and `GetReceipts` (unknown and duplicate hashes, over-limit counts,
the 2 `MiB` soft limit); frame and message size limits; snappy edge cases; unknown and reserved
ids; unsolicited responses; and broadcasts. Each scenario records the response, the Disconnect
reason, a close or a timeout, then sends a liveness Ping and reconnects with the same key to
detect bans. Each scenario uses its own 127.1.x.y source address.

`discv5` is a scripted discv5 v5.1 peer (masking, WHOAREYOU handshake, AES-GCM written from the
wire spec). It covers malformed headers (protocol id `basev0` vs `discv5`, version, flag,
authdata size, packet size 62/1280/1281), unsolicited WHOAREYOU and handshake packets, handshake
faults (bad id signature, garbage ephemeral key, undecryptable message, missing record), ENRs
(bad signature, other key, unsorted keys, 250/300/301/600 bytes) together with a probe that checks
whether the record entered the routing table, FINDNODE distances (0, 256, 257, `u64::MAX`, none,
3, 4, duplicates, malformed), and PING and TALKREQ variants, including enr-seq handling.
Requests that a target starts on its own (lookups) are listed apart from its answers.

Findings are written to `findings/el/`, which is not committed (confidential).
