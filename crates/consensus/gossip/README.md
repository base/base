# `base-consensus-gossip`

## Overview

GossipSub-based P2P networking for Base consensus. `GossipDriver` manages a libp2p swarm
that subscribes to unsafe block topics and propagates validated payloads. `BlockHandler`
validates incoming blocks against the rollup config, `ConnectionGater` enforces IP-based rate
limits and peer bans, and Prometheus metrics are recorded via `Metrics`.

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
