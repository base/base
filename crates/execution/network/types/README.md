# `base-execution-network-types`

Network Types and Utilities.

This crate manages and converts Ethereum network entities such as node records, peer IDs, and
Ethereum Node Records (ENRs)

## An overview of Node Record types

Ethereum uses different types of "node records" to represent peers on the network.

The simplest way to identify a peer is by public key. This is the [`PeerId`] type, which usually
represents a peer's secp256k1 public key.

A more complete representation of a peer is the [`NodeRecord`] type, which includes the peer's
IP address, the ports where it is reachable (TCP and UDP), and the peer's public key. This is
what is returned from discovery v4 queries.

The most comprehensive node record type is the Ethereum Node Record ([`Enr`]), which is a
signed, versioned record that includes the information from a [`NodeRecord`] along with
additional metadata. This is the data structure returned from discovery v5 queries.

When we need to deserialize an identifier that could be a [`PeerId`], [`NodeRecord`], [`Enr`],
or [`TrustedPeer`], we use the [`AnyNode`] type. [`AnyNode`] is used in reth's
`admin_addTrustedPeer` RPC method.

The __final__ type is the [`TrustedPeer`] type, which is similar to a [`NodeRecord`] but may
include a domain name instead of a direct IP address. It includes a `resolve` method, which can
be used to resolve the domain name, producing a [`NodeRecord`]. This is useful for adding
trusted peers at startup, whose IP address may not be static each time the node starts. This is
common in orchestrated environments like Kubernetes, where there is reliable service discovery,
but services do not necessarily have static IPs.

In short, the types are as follows:
- [`PeerId`]: A simple public key identifier.
- [`NodeRecord`]: A more complete representation of a peer, including IP address and ports.
- [`Enr`]: An Ethereum Node Record, which is a signed, versioned record that includes additional
  metadata. Useful when interacting with discovery v5, or when custom metadata is required.
- [`AnyNode`]: An enum over [`PeerId`], [`NodeRecord`], [`Enr`], and [`TrustedPeer`], useful in
  deserialization when the type of the node record is not known.
- [`TrustedPeer`]: A [`NodeRecord`] with an optional domain name, which can be resolved to a
  [`NodeRecord`]. Useful for adding trusted peers at startup, whose IP address may not be
  static.


## Feature Flags

- `net`: Support for address lookups.

