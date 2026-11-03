# `base-consensus-disc`

Discovery service for Base.

This crate provides decentralized peer discovery capabilities using the Discv5 distributed
hash table (DHT) protocol, as defined in the Ethereum networking specifications.

## Overview

The discovery service enables Base nodes to find and connect to other network
participants without relying on centralized infrastructure. It maintains a local
view of the network through ENRs (Ethereum Node Records) and facilitates peer
connections for the gossip layer.

## Key Components

- [`Discv5Driver`]: Main service driver that manages the discovery process
- [`Discv5Builder`]: Builder pattern for configuring discovery service parameters
- [`Discv5Handler`]: Handle for interacting with the discovery service
- [`LocalNode`]: Represents the local node's discovery information

## Discovery Process

1. **Bootstrap**: Connect to known bootstrap nodes to join the network
2. **Table Population**: Discover peers through DHT queries and populate the routing table
3. **Peer Maintenance**: Periodically refresh peer information and prune stale entries
4. **ENR Updates**: Keep local ENR information current and propagate changes

## ENR Management

ENRs (Ethereum Node Records) contain essential information about network peers:
- Node identity and cryptographic proof
- Network address and port information
- Protocol capabilities and version
- Chain-specific information (chain ID, etc.)

## ENR Trees (EIP-1459 DNS Discovery)

Besides fixed bootnodes, the driver can sync bootnodes from EIP-1459 ENR trees: lists of signed
node records published as DNS TXT records under a domain. Pass a tree in the bootnode list as
`enrtree://<pubkey>@<domain>`, where `<pubkey>` is the base32-encoded compressed secp256k1 key that
signs the tree root. For example:

```text
--p2p.bootnodes enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@nodes.example.org
```

`EnrTreeSync` wraps reth's `reth-dns-discovery` service. It verifies the root signature, walks the
tree, and rechecks the root every 30 minutes, re-syncing when the sequence number changes. Until the
tree yields its first record, it restarts the sync every 60 seconds. That covers a root lookup
that failed and records that did not resolve. After that, records that failed to resolve are
retried only once the tree's root changes. It yields only
records that pass `EnrValidation` for the node's chain ID, so the tree may contain other records.
Yielded ENRs are added to the discv5 table, forwarded to gossip, and stored in the bootstore.

An unreachable or invalid tree never stops the node: failures are logged and the other bootnodes
keep working. The sync has no task of its own: it is polled inside the driver task and dropped
with it, which happens at runtime shutdown.

Publishing a tree is out of scope for this crate. Use geth's `devp2p dns` tool:
`devp2p dns sign` builds and signs a tree from a node list, and `devp2p dns to-route53` or
`devp2p dns to-cloudflare` deploys the TXT records.

## Persistent Storage

The service maintains a persistent bootstore that caches discovered peers across
restarts, reducing bootstrap time and improving network resilience.

## Configuration

Key configuration parameters include:
- Discovery interval for random peer queries
- Bootstrap node list and ENR trees (`Discv5Builder::with_enr_trees`)
- Storage location for persistent peer cache
- Network interface and port bindings

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-disc = { workspace = true }
```

Construct and run the discovery service via `Discv5Builder`. Call `start` inside a Tokio
runtime because it spawns a task:

```rust,ignore
use base_consensus_disc::Discv5Builder;

let driver = Discv5Builder::new(local_node, chain_id, discovery_config).build()?;

let (handler, enr_rx) = driver.start();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
