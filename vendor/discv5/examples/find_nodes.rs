//! Demonstrates how to run a basic Discovery v5 Service.
//!
//! This example creates a discv5 service which searches for peers every 30 seconds. On creation,
//! the local ENR created for this service is displayed in base64. This can be used to allow other
//! instances to connect and join the network. The service can be stopped by pressing Ctrl-C.
//!
//! To add peers to the network, create multiple instances of this service adding the ENR of a
//! participating node in the command line. The nodes should discover each other over a period of
//! time. (It is probabilistic that nodes to find each other on any given query).
//!
//! See the example's help with
//! ```
//! sh cargo run --example find_nodes -- --help
//! ```
//!
//! For a simple CLI discovery service see [discv5-cli](https://github.com/AgeManning/discv5-cli)

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use clap::Parser;
use discv5::{
    ConfigBuilder, Discv5, Event, ListenConfig, enr,
    enr::{CombinedKey, k256},
};
use tracing::{info, warn};

#[derive(Parser)]
struct FindNodesArgs {
    /// Type of socket to bind ['ds', 'ip4', 'ip6'].
    #[clap(long, default_value_t = SocketKind::Ds)]
    socket_kind: SocketKind,
    /// IpV4 to advertise in the ENR. This is needed so that other IpV4 nodes can connect to us.
    #[clap(long)]
    enr_ip4: Option<Ipv4Addr>,
    /// IpV6 to advertise in the ENR. This is needed so that other IpV6 nodes can connect to us.
    #[clap(long)]
    enr_ip6: Option<Ipv6Addr>,
    /// Port to bind. If none is provided, a random one in the 9000 - 9999 range will be picked
    /// randomly.
    #[clap(long)]
    port: Option<u16>,
    /// Port to bind for ipv6. If none is provided, a random one in the 9000 - 9999 range will be picked
    /// randomly.
    #[clap(long)]
    port6: Option<u16>,
    /// Use a default test key.
    #[clap(long)]
    use_test_key: bool,
    /// A remote peer to try to connect to. Several peers can be added repeating this option.
    #[clap(long)]
    remote_peer: Vec<discv5::Enr>,
    /// Use this option to turn on printing events received from discovery.
    #[clap(long)]
    events: bool,
}

#[tokio::main]
async fn main() {
    let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();
    let _ = tracing_subscriber::fmt().with_env_filter(filter_layer).try_init();

    let args = FindNodesArgs::parse();
    let port = args.port.unwrap_or_else(|| (rand_08::random::<u16>() % 1000) + 9000);
    let port6 = args.port.unwrap_or_else(|| {
        loop {
            let port6 = (rand_08::random::<u16>() % 1000) + 9000;
            if port6 != port {
                return port6;
            }
        }
    });

    let enr_key = if args.use_test_key {
        // A fixed key for testing
        let raw_key =
            hex::decode("b71c71a67e1177ad4e901695e1b4b9ee17ae16c6668d313eac2f96dbcda3f291")
                .unwrap();
        let secret_key = k256::ecdsa::SigningKey::from_slice(&raw_key).unwrap();
        CombinedKey::from(secret_key)
    } else {
        // use a new key if specified
        CombinedKey::generate_secp256k1()
    };

    let enr = {
        let mut builder = enr::Enr::builder();
        if let Some(ip4) = args.enr_ip4 {
            // if the given address is the UNSPECIFIED address we want to advertise localhost
            if ip4.is_unspecified() {
                builder.ip4(Ipv4Addr::LOCALHOST).udp4(port);
            } else {
                builder.ip4(ip4).udp4(port);
            }
        }
        if let Some(ip6) = args.enr_ip6 {
            // if the given address is the UNSPECIFIED address we want to advertise localhost
            if ip6.is_unspecified() {
                builder.ip6(Ipv6Addr::LOCALHOST).udp6(port6);
            } else {
                builder.ip6(ip6).udp6(port6);
            }
        }
        builder.build(&enr_key).unwrap()
    };

    // the address to listen on.
    let listen_config = match args.socket_kind {
        SocketKind::Ip4 => ListenConfig::from_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        SocketKind::Ip6 => ListenConfig::from_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port6),
        SocketKind::Ds => ListenConfig::default()
            .with_ipv4(Ipv4Addr::UNSPECIFIED, port)
            .with_ipv6(Ipv6Addr::UNSPECIFIED, port6),
    };

    // default configuration with packet filtering
    // let config = ConfigBuilder::new(listen_config).enable_packet_filter().build();

    // default configuration without packet filtering
    let config = ConfigBuilder::new(listen_config).build();

    info!("Node Id: {}", enr.node_id());
    if args.enr_ip6.is_some() || args.enr_ip4.is_some() {
        // if the ENR is useful print it
        info!(
            base64_enr = &enr.to_base64(),
            ipv6_socket = ?enr.udp6_socket(),
            ipv4_socket = ?enr.udp4_socket(),
            "Local ENR",
        );
    }

    // construct the discv5 server
    let mut discv5: Discv5 = Discv5::new(enr, enr_key, config).unwrap();

    // if we know of another peer's ENR, add it known peers
    for enr in args.remote_peer {
        info!(
            udp4_socket = ?enr.udp4_socket(),
            udp6_socket = ?enr.udp6_socket(),
            tcp4_port = ?enr.tcp4(),
            tcp6_port = ?enr.tcp6(),
            "Remote ENR read",
        );
        if let Err(e) = discv5.add_enr(enr) {
            warn!(error = ?e, "Failed to add remote ENR");
            // It's unlikely we want to continue in this example after this
            return;
        };
    }

    // start the discv5 service
    discv5.start().await.unwrap();
    let mut event_stream = discv5.event_stream().await.unwrap();
    let check_evs = args.events;

    // construct a 30 second interval to search for new peers.
    let mut query_interval = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = query_interval.tick() => {
                // pick a random node target
                let target_random_node_id = enr::NodeId::random();
                // get metrics
                let metrics = discv5.metrics();
                let connected_peers = discv5.connected_peers();
                info!(
                    connected_peers,
                    active_sessions = metrics.active_sessions,
                    unsolicited_requests_per_second = format_args!("{:.2}", metrics.unsolicited_requests_per_second),
                    "Searching for peers..."
                );
                // execute a FINDNODE query
                match discv5.find_node(target_random_node_id).await {
                    Err(e) => warn!(error = ?e, "Find Node result failed"),
                    Ok(v) => {
                        // found a list of ENR's print their NodeIds
                        let node_ids = v.iter().map(|enr| enr.node_id()).collect::<Vec<_>>();
                        info!(len = node_ids.len(), "Nodes found");
                        for node_id in node_ids {
                            info!(%node_id, "Node");
                        }
                    }
                }
            }
            Some(discv5_ev) = event_stream.recv() => {
                // consume the events even if not printed
                if !check_evs {
                    continue;
                }
                match discv5_ev {
                    Event::Discovered(enr) => info!(%enr, "Enr discovered"),
                    Event::NodeInserted { node_id, replaced: _ } => info!(%node_id, "Node inserted"),
                    Event::SessionEstablished(enr, _) => info!(%enr, "Session established"),
                    Event::SocketUpdated(addr) => info!(%addr, "Socket updated"),
                    Event::TalkRequest(_) => info!("Talk request received"),
                    _ => {}
                };
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum SocketKind {
    Ip4,
    Ip6,
    Ds,
}

impl std::fmt::Display for SocketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketKind::Ip4 => f.write_str("ip4"),
            SocketKind::Ip6 => f.write_str("ip6"),
            SocketKind::Ds => f.write_str("ds"),
        }
    }
}

impl std::str::FromStr for SocketKind {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ip4" => Ok(SocketKind::Ip4),
            "ip6" => Ok(SocketKind::Ip6),
            "ds" => Ok(SocketKind::Ds),
            _ => Err("bad kind"),
        }
    }
}
