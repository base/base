discv5
============

[![Build Status]][Build Link] [![Doc Status]][Doc Link] [![Crates
Status]][Crates Link]

[Build Status]: https://github.com/sigp/discv5/workflows/build/badge.svg?branch=master
[Build Link]: https://github.com/sigp/discv5/actions
[Doc Status]: https://docs.rs/discv5/badge.svg
[Doc Link]: https://docs.rs/discv5
[Crates Status]: https://img.shields.io/crates/v/discv5.svg
[Crates Link]: https://crates.io/crates/discv5

[Documentation at docs.rs](https://docs.rs/discv5)


# Overview

This is a rust implementation of the [Discovery v5](https://github.com/ethereum/devp2p/blob/master/discv5/discv5.md)
peer discovery protocol.

Discovery v5 is a protocol designed for encrypted peer discovery. Each peer/node on the network is
identified via it's `ENR` ([Ethereum Node Record](https://eips.ethereum.org/EIPS/eip-778)), which
is essentially a signed key-value store containing the node's public key and optionally IP address
and port.

Discv5 employs a kademlia-like routing table to store and manage discovered peers and topics. The
protocol allows for external IP discovery in NAT environments through regular PING/PONG's with
discovered nodes. Nodes return the external IP address that they have received and a simple
majority is chosen as our external IP address. If an external IP address is updated, this is
produced as an event to notify the swarm (if one is used for this behaviour).

For a simple CLI discovery service see [discv5-cli](https://github.com/AgeManning/discv5-cli)

# Usage

A simple example of creating this service is as follows:

```rust
   use discv5::{enr, enr::{CombinedKey, NodeId}, TokioExecutor, Discv5, ConfigBuilder};
   use discv5::socket::ListenConfig;
   use std::net::SocketAddr;

   // construct a local ENR
   let enr_key = CombinedKey::generate_secp256k1();
   let enr = enr::Enr::empty(&enr_key).unwrap();

   // build the tokio executor
   let mut runtime = tokio::runtime::Builder::new_multi_thread()
       .thread_name("Discv5-example")
       .enable_all()
       .build()
       .unwrap();

   // configuration for the sockets to listen on
   let listen_config = ListenConfig::Ipv4 {
       ip: Ipv4Addr::UNSPECIFIED,
       port: 9000,
   };

   // default configuration
   let config = ConfigBuilder::new(listen_config).build();

   // construct the discv5 server
   let mut discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();

   // In order to bootstrap the routing table an external ENR should be added
   // This can be done via add_enr. I.e.:
   // discv5.add_enr(<ENR>)

   // start the discv5 server
   runtime.block_on(discv5.start());

   // run a find_node query
   runtime.block_on(async {
      let found_nodes = discv5.find_node(NodeId::random()).await.unwrap();
      println!("Found nodes: {:?}", found_nodes);
   });
```

# Addresses in ENRs 

An ENR is a signed record which is primarily used in this protocol for
identifying and connecting to peers. ENRs have **OPTIONAL** `ip` and `port`
fields.

If a node does not know its contactable address (i.e if it is behind a NAT), it should leave these fields
empty. This is done for the following reasons:
1. When we receive an ENR we must decide whether to add it to our local routing
   table and advertise it to other peers. If a node has put some
   non-contactable address in the ENR (e.g `127.0.0.1` when connecting to
   non-local nodes) we cannot use this ENR
   to contact the node and we therefore do not wish to advertise it to other
   nodes. Putting a non-contactable address is therefore functionally
   equivalent to leaving the fields empty.
2. For every new inbound connection, we do not wish to check that the address
   given to us in an `ENR` is contactable. We do not want the scenario, where
   any peer can give us any address and force us to attempt a connection to
   arbitrary addresses (to check their validity) as it consumes unnecessary
   bandwidth and we want to avoid DOS attacks where malicious users spam many
   nodes attempting them all to send messages to a victim IP.

## How this protocol handles advertised IPs in ENRs

To handle the above two cases this protocol filters out and only advertises
contactable ENRs. It doesn't make sense for a discovery protocol to advertise
non-contactable peers.

This is done in the following way:

1. If a connecting node provides an ENR without specifying an address (this
   should be the default case for most nodes behind a NAT, or ones that have
   just started) we consider this valid. Typically this will occur when a node
   has yet to determine its external IP address via PONG responses and has not
   updated its ENR to a contactable address. In this case, we respond to all
   requests this peer asks for but we do not store or add its ENR to our
   routing table.
2. If a peer connects to us with an ENR that specifies an IP address that
   matches the src address we received the packet from, we consider this peer
   valid and attempt to add it to our local routing table and therefore may advertise
   its ENR to others.
3. If a peer connects to us with an ENR that specifies an IP address that does
   not match the src socket it connects to us on (e.g `127.0.0.1`, or
   potentially some internal subnet IP that is unreachable from our current
   network) we consider this ENR invalid and will not add it to our routing
   table. However we will still respond to discovery requests.
