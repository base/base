# Devnet Chaos Scenarios

This guide covers `just devnet chaos <scenario>`, which injects faults (chaos testing) into the running
local Docker devnet and reports how the chain reacts. Each scenario breaks one component, shows the
heads the fault should move, then restores the component and checks that the chain recovers. Only
`batcher-outage` is implemented today; the rest of the list is the plan.

Start a devnet first with `just devnet up` or `just devnet up-single` (see [DEVNET.md](DEVNET.md)).
Every scenario refuses to run unless the containers it needs are running in the local devnet compose
project, and none of them changes the devnet's configuration.

| Scenario | Status | Fault | Expected chain behavior |
|---|---|---|---|
| `batcher-outage [secs]` | Implemented | Stops the canonical `op-batcher` | Safe head stalls, unsafe head keeps advancing, safe head catches up after restart |
| `sequencer-failover` | Planned | Transfers conductor leadership away from the active sequencer (HA) | Unsafe head pauses for a block or two, then continues on the new leader without a reorg |
| `sequencer-halt [secs]` | Planned | Stops sequencing on the only sequencer (single) | Unsafe head stalls, safe head catches up to it and stops, both resume after restart |
| `validator-partition [secs]` | Planned | Cuts a validator's P2P peers | Validator's unsafe head stalls while its safe head follows L1; unsafe catches up on reconnect |
| `node-restart <service>` | Planned | Restarts one L2 node container | Node falls behind, then resyncs to the cluster's heads |
| `l1-rpc-outage [secs]` | Planned | Makes the L1 JSON-RPC unavailable to the L2 nodes | Derivation and batching stop, so safe heads stall; unsafe continues on a fixed L1 origin |
| `l1-reorg [depth]` | Planned | Replaces recent L1 blocks with an empty branch | Safe heads derived from the dropped blocks reorg back, then re-derive once batches are reposted |

## Signals

Each node's consensus RPC answers `optimism_syncStatus`. The fields the scenarios watch are:

- `unsafe_l2`: the newest L2 block the node has, from its own sequencing or from P2P gossip.
- `safe_l2`: the newest L2 block derived from batch data on L1.
- `finalized_l2`: the newest safe block whose L1 data is finalized.
- `current_l1` and `head_l1`: the L1 block derivation has reached and the L1 head the node sees.

```sh
curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"optimism_syncStatus","params":[],"id":1}' \
  http://localhost:7549 | jq '.result | {unsafe: .unsafe_l2.number, safe: .safe_l2.number, l1: .current_l1.number}'
```

The builder's consensus RPC is `http://localhost:7549`, the client's is `http://localhost:8549`, and the
shadow validator's is `http://localhost:8849`. `just devnet status` prints the builder's and client's
heads, and the Devnet dashboard in Grafana (`http://localhost:3000`) charts the heads and the gaps
between them.

The client derives `BASE_NODE_VERIFIER_L1_CONFS` (15) L1 blocks behind the L1 head, so its safe head
reacts to every L1-side fault 15 L1 blocks after the builder's.

## Implemented

### `batcher-outage [secs]`

```sh
just devnet chaos batcher-outage 60
```

Stops the canonical `op-batcher` container for `secs` seconds (default 60) and then starts it again.
The container stops with SIGKILL, so this models a batcher crash rather than a graceful drain. The
recipe restarts the batcher on any exit, including Ctrl+C.

What the chain does:

- The sequencer keeps producing blocks, so `unsafe_l2` keeps advancing on every node.
- No new batches reach the canonical batch inbox, so `safe_l2` stops advancing on the builder and,
  15 L1 blocks later, on the client. A batch transaction already in the L1 mempool can still land and
  move the safe head once after the stop.
- After the restart, `op-batcher` resumes from the safe head and posts the backlog. The builder's
  `safe_l2` catches up to the unsafe head recorded at the end of the outage within a few L1 blocks.
- The shadow `base-batcher` keeps running, so the shadow validator's safe head keeps advancing
  during the outage, although it can trail the canonical safe head. It is the control that shows L1
  inclusion and derivation still work.

The recipe prints the L1 head, the builder's unsafe and safe heads, the gap between them (safe lag),
and the client's safe head every two seconds. It fails if the unsafe head did not advance during the
outage, or if the builder's safe head does not reach the end-of-outage unsafe head within `secs` plus
300 seconds of the restart. It warns when the safe lag did not grow, which happens when the outage is
shorter than the normal batch interval of a few L1 blocks; use at least 60 seconds.

An outage longer than the sequencing window would make derivation build deposit-only safe blocks and
reorg the unsafe chain. The devnet's sequencing window is hours long, so this scenario does not reach
that case.

## Planned

These scenarios reuse fault injection that already exists in the repository but is not yet reachable
through `just devnet chaos`.

### `sequencer-failover`

Wraps `just devnet transfer-leader` on the HA devnet, which asks op-conductor to transfer raft
leadership. Watch `conductor_leader` on the conductor RPCs (6545 to 6547) and the unsafe head on every
sequencer: it should pause briefly and continue on the new leader with no reorg and no duplicate block
numbers. The safe head is unaffected because the batchers read blocks through the conductors.
`basectl conductor` and `basectl sequencer` ([bin/basectl/README.md](../../bin/basectl/README.md))
provide the same operations plus `pause` and `unpause` of the conductor control loop.

### `sequencer-halt [secs]`

Calls `admin_stopSequencer` on the single-sequencer devnet and `admin_startSequencer` afterwards, the
RPCs that `basectl sequencer stop` and `start` call on HA clusters. The unsafe head stops; the batcher drains the remaining blocks,
so the safe head catches up to the unsafe head and stops too. After the restart both resume from the
same block. Watch that the restarted sequencer continues from its last unsafe block.

### `validator-partition [secs]`

Removes the client's execution-layer and consensus-layer peers (`admin_removePeer`,
`opp2p_disconnectPeer`, and `opp2p_blockPeer`), as the pause action in basectl's conductor TUI does
for sequencers (`crates/infra/basectl/src/rpc/admin.rs`). The client's `unsafe_l2` stops following
gossip while its `safe_l2` keeps advancing from L1. On reconnect, the unsafe head should catch up
through gossip and execution-layer sync. Watch `opp2p_peers` and the gap between the client's and the
builder's unsafe heads.

### `node-restart <service>`

Restarts one L2 node container, such as `base-client` or `base-rpc`, the way `basectl`'s conductor TUI
restarts a cluster node. Watch the node's heads fall behind and return to the builder's, and that the
safe head it reports after restart is not lower than before.

### `l1-rpc-outage [secs]`

Puts a controllable proxy in front of the L1 RPC, like the `L1RpcProxy` that system tests use
(`etc/systems/src/l1/rpc_proxy.rs`). With L1 unavailable, derivation stops at `current_l1`, so safe heads
stall, and the batchers cannot post. The sequencer keeps building on its last L1 origin until it hits
the maximum sequencer drift. The devnet nodes connect to L1 directly, so this scenario needs a proxy in
the compose network before it can be implemented.

### `l1-reorg [depth]`

Builds an empty replacement L1 branch through the L1 execution client's authenticated Engine API, like
the `L1ReorgDriver` that system tests use (`etc/systems/src/l1/reorg.rs`). Safe L2 blocks derived from
the dropped L1 blocks should reorg back to the last safe block whose data survived; the unsafe chain
stays, and the batcher reposts the dropped batches so the safe head re-derives. Watch `safe_l2.hash`
before and after, not only its number. The devnet's Lighthouse beacon node also drives the L1 execution
client, so this scenario must keep the two from fighting over the fork choice.
