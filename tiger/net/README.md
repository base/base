# base-tiger-net

Peer-to-peer networking and execution-layer (EL) sync for the tiger client: discv5 node
discovery with Base's identity, RLPx transport (ECIES handshake, framing, Snappy), the eth/69
and snap/2 wire protocols, peer management, a header and body downloader for backfill, and a
snap/2 state syncer with healing, plus serving all of these to peers. It speaks to base-reth
peers the way base-reth speaks to them. No reth, revm or async runtime: std threads, blocking
sockets with timeouts, and channels.

## What the node provides

- [`BlockSource`]: the chain head (number, hash, timestamp, earliest served block), headers by
  hash and number, bodies and receipts by hash. The network serves eth requests and builds its
  Status and fork id from it.
- [`SnapStateSource`] (optional): state views by state root (`open_state`) and code by hash,
  used to serve snap requests. Without it, snap requests get empty answers.
- [`SnapStateSink`]: where snap sync writes. `commit_snap(&SnapBatch)` commits accounts,
  storage, code, completed key ranges, pending-storage accounts and the syncer's progress blob in
  one transaction; `finish_snap(head, root)` rebuilds the tries and checks the root.

## What the node calls

- `Network::start(NetworkConfig, Arc<dyn BlockSource>, Option<Arc<dyn SnapStateSource>>)`.
  `NetworkConfig::new(secret_key, network_id, ForkSchedule::base(..))` sets the defaults
  (100 outbound, 30 inbound, TCP and UDP port 30303); set `bootnodes`, `trusted_peers`, `listen_addr`, and
  `discovery_addr` (`None` disables discovery).
- `Downloader::new(network, DownloadConfig::default())`:
  - `headers_back(target_hash, stop_at, max)`: headers from the target back to `stop_at`,
    descending and linked by parent hash (backfill of a forkchoice to an unknown head);
  - `headers(HeadersRequest)`: a range by number or hash;
  - `bodies(&headers, progress)`: bodies validated against the transactions root, ommers hash
    and withdrawals root.
- `SnapSync::new(network, sink, SnapSyncConfig::default())`, then `run(progress)` on its own
  thread. Before writing each executed block's state diff, the node calls
  `SnapHandle::on_block(number, state_root, &TouchedKeys)` and holds the returned guard while it
  writes. Snap then drops downloaded keys that a newer block touched, so the node's own diffs
  heal the moving pivot, and snap/2 needs no GetTrieNodes.

## Flags

| base-reth flag | `NetworkConfig` |
| --- | --- |
| `--port` | `listen_addr` |
| `--discovery.v5.port` | `discovery_addr` |
| `--disable-discovery` | `discovery_addr = None` |
| `--bootnodes` | `bootnodes` (empty by default; Base's chain bootnodes are [`BASE_BOOTNODES`]) |
| `--trusted-peers` | `trusted_peers`: always dialed, never banned |
| `--max-outbound-peers`, `--max-inbound-peers` | `max_outbound`, `max_inbound` |
| `--p2p-secret-key` | `secret_key` |
| `--nat extip:` | `external_ip` |

## Examples

`examples/probe.rs` connects to a peer (or discovers one), exchanges Status and downloads a
range of headers and bodies. `examples/snap_probe.rs` snap syncs a peer's state into memory and
checks the root, optionally following a moving head over RPC to exercise healing.
