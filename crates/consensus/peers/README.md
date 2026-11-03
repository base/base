# `base-consensus-peers`

<a href="https://crates.io/crates/base-consensus-peers"><img src="https://img.shields.io/crates/v/base-consensus-peers.svg?label=base-consensus-peers&labelColor=2a2f35" alt="base-consensus-peers"></a>

## Overview

Networking peer types ported from reth. Provides `PeerId` and `NodeRecord` for working with
Ethereum network identities across discv4 and discv5.

Networking Utilities ported from reth.

Much of this module is ported from
<https://github.com/paradigmxyz/reth/blob/0e087ae1c35502f0b8d128c64e4c57269af20c0e/crates/net/peers/src/lib.rs>.

This module manages and converts Ethereum network entities such as node records, peer IDs, and
Ethereum Node Records (ENRs)

## Node Record Overview

Ethereum uses different types of "node records" to represent peers on the network.

The simplest way to identify a peer is by public key. This is the `PeerId` type, which usually
represents a peer's secp256k1 public key.

A more complete representation of a peer is the `NodeRecord` type, which includes the peer's
IP address, the ports where it is reachable (TCP and UDP), and the peer's public key. This is
what is returned from discovery v4 queries.

The most comprehensive node record type is the Ethereum Node Record (`discv5::Enr`), which is
a signed, versioned record that includes the information from a `NodeRecord` along with
additional metadata. This is the data structure returned from discovery v5 queries.

In short, the types are as follows:
- `PeerId`: A simple public key identifier.
- `NodeRecord`: A more complete representation of a peer, including IP address and ports.
- `discv5::Enr`: An Ethereum Node Record, which is a signed, versioned record that includes
  additional metadata. Useful when interacting with discovery v5, or when custom metadata is
  required.

## Bootnode Formats and DNS Hostnames

`BootNodeEntry` parses one configured bootnode without any network access:

- `enr:...`: a signed ENR, used as is.
- `enode://<id>@<ip>:<port>[?discport=<port>]`: an unsigned node record with an IP address.
- `enode://<id>@<hostname>:<port>[?discport=<port>]`: an `EnodeUrl` whose host is a DNS hostname,
  the consensus-layer counterpart of reth's `TrustedPeer`.

`BootNodes::resolve` turns the entries into `BootNodes`. It resolves hostnames concurrently
through a `HostResolver` (the OS resolver in production via `SystemHostResolver`), bounds each
lookup by a timeout, and uses the first address returned, as reth does. A hostname that fails
to resolve is skipped with a `warn!` that names the host and the error. Resolution fails only
when bootnodes were configured, none of them is usable, and the caller has no other way to seed
discovery, such as chain default bootnodes.

Hostnames are resolved once, at startup. `NodeRecord::from_str` rejects hostnames with
`NodeRecordParseError::UnresolvedHost` instead of performing a blocking lookup.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-peers = { workspace = true }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
