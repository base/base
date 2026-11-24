# base-tiger-net

Peer-to-peer networking and execution-layer (EL) sync for the tiger client: discv5 node
discovery with Base's identity, RLPx transport (ECIES handshake, framing, Snappy), the eth/69
wire protocol, peer management, and a header and body downloader for backfill, plus serving
headers, bodies and receipts to peers. It speaks to base-reth peers the way base-reth speaks to
them. Snap sync is not implemented: base-reth does not use it and Base does not support it, so
the node executes downloaded blocks to build state. No reth, revm or async runtime: std threads,
blocking sockets with timeouts, and channels.

## What the node provides

- [`BlockSource`]: the chain head (number, hash, timestamp, earliest served block), headers by
  hash and number, bodies and receipts by hash. The network serves eth requests and builds its
  Status and fork id from it.

## What the node calls

- `Network::start(NetworkConfig, Arc<dyn BlockSource>)`.
  `NetworkConfig::new(secret_key, network_id, ForkSchedule::base(..))` sets the defaults
  (100 outbound, 30 inbound, TCP and UDP port 30303); set `bootnodes`, `trusted_peers`,
  `listen_addr`, and `discovery_addr` (`None` disables discovery).
- `Downloader::new(network, DownloadConfig::default())`:
  - `headers_back(target_hash, stop_at, max)`: headers from the target back to `stop_at`,
    descending and linked by parent hash (backfill of a forkchoice to an unknown head);
  - `headers(HeadersRequest)`: a range by number or hash;
  - `bodies(&headers, progress)`: bodies validated against the transactions root, ommers hash
    and withdrawals root.

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
range of headers and bodies.
