# `base-consensus-gossip`

Gossip protocol implementation for Base.

This crate provides a comprehensive gossip networking implementation for Base,
including GossipSub-based consensus layer networking, RPC interfaces for network
administration, and metrics collection.

## Key Components

- [`GossipDriver`]: Main driver managing the libp2p swarm and event handling
- [`Behaviour`]: Custom libp2p behavior combining `GossipSub`, Ping, and Identify
- [`BlockHandler`]: Validates and processes incoming block payloads
- [`ConnectionGater`]: Sophisticated connection management and rate limiting
- [`P2pRpcRequest`]: RPC interface for network administration
- [`Metrics`]: Metrics collection for monitoring and observability

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
