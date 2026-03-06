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

## Persistent Storage

The service maintains a persistent bootstore that caches discovered peers across
restarts, reducing bootstrap time and improving network resilience.

## Configuration

Key configuration parameters include:
- Discovery interval for random peer queries
- Bootstrap node list
- Storage location for persistent peer cache
- Network interface and port bindings
