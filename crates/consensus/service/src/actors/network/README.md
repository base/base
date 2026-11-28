# Network actor

The network actor owns the consensus node's P2P boundary: libp2p gossipsub
for unsafe-block propagation and discv5 for peer discovery. Production code
constructs it from a `NetworkBuilder`; tests can bypass sockets by calling
`NetworkActor::with_transport` with an in-process `GossipTransport`.

### Example

> **Warning**
>
> Bind and advertise an outward-facing address when joining a real network.
> Using `127.0.0.1` or `localhost` prevents other peers from connecting back
> to your node.

```rust,ignore
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use alloy_primitives::address;
use base_common_genesis::RollupConfig;
use base_consensus_disc::LocalNode;
use base_consensus_service::{
    NetworkActor, NetworkBuilder, NetworkConfig, NetworkEngineClient, NodeActor,
};
use discv5::enr::CombinedKey;
use libp2p::{Multiaddr, multiaddr::Protocol};
use tokio_util::sync::CancellationToken;

async fn run_network<E>(engine_client: E) -> eyre::Result<()>
where
    E: NetworkEngineClient + 'static,
{
    let unsafe_block_signer = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let gossip = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9099);
    let mut gossip_addr = Multiaddr::empty();
    gossip_addr.push(Protocol::Ip4(Ipv4Addr::UNSPECIFIED));
    gossip_addr.push(Protocol::Tcp(gossip.port()));

    let CombinedKey::Secp256k1(k256_key) = CombinedKey::generate_secp256k1() else {
        unreachable!();
    };
    let discovery = LocalNode::new(k256_key, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9097, 9098);

    let config = NetworkConfig::new(
        RollupConfig::default(),
        discovery,
        gossip_addr,
        unsafe_block_signer,
    );
    let builder = NetworkBuilder::from(config);
    let cancellation = CancellationToken::new();

    let (inbound, network) =
        NetworkActor::new(engine_client, cancellation.clone(), builder).await?;

    tokio::spawn(network.start(()));

    // Other actors use these senders to update signer state, publish unsafe
    // payloads, and route P2P/admin RPC requests into the network actor.
    inbound.signer.send(unsafe_block_signer).await?;

    Ok(())
}
```

## tp2p gossip transport

With the default-off `tp2p` Cargo feature (`base-consensus-node/tp2p`,
`base-consensus-cli/tp2p`, `base-consensus/tp2p`), unsafe payloads can also
travel over tp2p (`base-tiger-p2p`, `tiger/p2p`), an encrypted std-thread
peer-to-peer network, selected with `--p2p.tp2p <off|both|only>`:

- `off` (default): libp2p and discv5 only, as without the feature.
- `both`: libp2p as usual plus tp2p. Payloads are signed once and published on
  both; a follower takes them from both. Each transport validates with its own
  `BlockHandler`, so the networks keep independent seen-block state, and
  `SelectedGossipTransport` forwards a block hash to the engine only once.
- `only`: tp2p alone; libp2p and discv5 are not started, and the P2P RPC
  API (`opp2p_*`) gets no answers.

A tp2p gossip message's topic is the payload version (0 to 3) and its data is
exactly what libp2p publishes on `/optimism/{chain_id}/{version}/blocks`.
`Tp2pTransport` runs every inbound message through `BlockHandler::handle`
under that libp2p topic (higher topic bytes become unknown topics and are
rejected) and returns its verdict to tp2p, which penalizes senders of rejected
messages. The tp2p network is bound to the L2 chain id and genesis hash.

Flags: `--p2p.tp2p.listen` (default `0.0.0.0:9224`), `--p2p.tp2p.advertise`,
`--p2p.tp2p.bootnodes` and `--p2p.tp2p.trusted`
(`tp2p://<compressed pubkey hex>@host:port`, comma-separated), and
`--p2p.tp2p.priv-path` (default: the libp2p secp256k1 key). The node logs its
own `tp2p://` address at startup.
