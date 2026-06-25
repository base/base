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
