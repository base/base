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

`EnrTreeSync` walks the tree with reth's `reth-dns-discovery` tree types. The root must be signed
by the tree's key, and every entry must match the hash it is published under. It yields only records
that pass `EnrValidation` for the node's chain ID, so the tree may contain other records. Yielded
ENRs are added to the discv5 table, forwarded to gossip, and stored in the bootstore.

The walk is bounded, so a large or hostile tree cannot grow memory or per-poll work. The limits
apply to each configured tree:

| Limit | Value |
| --- | --- |
| Links to other trees (`enrtree-link`) | not followed |
| Lookups in flight | 1, at most 3 per second, each with a 5 second timeout |
| Distinct entries admitted per walk (also caps the pending queue) | 2,000 |
| Records yielded per walk | 500 |
| Lookups of one entry per walk, including retries | 3 |

A walk that hits a limit stops with a warning and keeps the records it found. Entries are visited
breadth-first, so in a tree larger than the limit, records past the first 2,000 entries are not
reached. A repeated entry hash is visited once. A failed entry is retried in place without
restarting the walk, and retries are not limited by the entry budget. A tree that publishes its
nodes only through links to other trees yields nothing; link each of those trees directly instead.

After a complete walk, the root is rechecked every 30 minutes and the tree is walked again only
when its ENR subtree hash changes. A walk is complete only when every admitted entry resolved and
matched its hash. A root that fails to resolve or verify, an incomplete walk, and a tree without
valid records are retried with a warning after a backoff that starts at 60 seconds and doubles up
to 30 minutes. A signed root whose sequence number is lower than one already accepted is ignored as
a rollback.

An unreachable or invalid tree never stops the node: the other bootnodes keep working. The sync has
no task of its own: it is polled inside the driver task and dropped with it, which happens at
runtime shutdown.

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
