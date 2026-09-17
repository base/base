# `base-consensus-gossip`

## Overview

GossipSub-based P2P networking for Base consensus. `GossipDriver` manages a libp2p swarm
that subscribes to unsafe block topics and propagates validated payloads. `BlockHandler`
validates incoming blocks against the rollup config, `ConnectionGater` enforces IP-based rate
limits and peer bans, and Prometheus metrics are recorded via `Metrics`.

Base does not advertise or implement the legacy op-node `payload_by_number` request-response
protocol. This does not affect execution-layer synchronization or HTTP follow-mode payload fetching.

Block topics retire according to the configured fork schedule once every pre-fork block is
outside the 60-second gossip age window. Startup skips already-retired topics, and the driver
checks for retirement every five seconds while running, including when idle. Inbound decoding
and outbound encoding enforce the cutoff immediately; future topics remain joined ahead of
activation. Retired subscriptions are not automatically restored after a clock rollback.

With the `metrics` feature, topic health is exported under `base_node_block_topic_*`:

| Metric suffix | Meaning |
| --- | --- |
| `subscribed` | Actual local subscription, 0 or 1, including topics skipped at startup |
| `peers` | Connected peers advertising the topic, even if it is locally retired |
| `mesh_peers` | Peers in the local topic mesh; distinct from total connections |
| `retirements_total` | Successful runtime unsubscriptions; startup skips are not counted |
| `blocked_total` | Inbound/outbound messages blocked by the topic guards, not all wire traffic |

Labels are bounded to `version=v1|v2|v3|v4` and, for blocked messages,
`direction=inbound|outbound`. State gauges are sampled at startup and every five seconds;
retirement immediately refreshes them. Unknown topics do not create label values. Monitor
V4 mesh peers alongside connected peers during retirement: leaving an old mesh does not
directly disconnect its peers, but an old-topic-only connection may subsequently become idle.

Coverage includes a real-TCP mixed-subscription peer test in this crate, production-handler
delivery into an action-test verifier (`unsafe_gossip`), and a Docker-backed system test
(`gossip_topic_retirement`) that checks peer-topic RPCs and unsafe sync with batching stopped.


## Key Components

- [`GossipDriver`]: Main driver managing the libp2p swarm and event handling
- [`Behaviour`]: Custom libp2p behavior combining `GossipSub`, Ping, and Identify
- [`BlockHandler`]: Validates and processes incoming block payloads
- [`ConnectionGater`]: Sophisticated connection management and rate limiting
- [`P2pRpcRequest`]: RPC interface for network administration
- [`Metrics`]: Metrics collection for monitoring and observability

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-gossip = { workspace = true }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
