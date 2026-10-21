//! Discovery-only bootnode command.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use base_node_core::BASE_V0_PROTOCOL_VERSION;
use clap::Parser;
use reth_cli_util::{get_secret_key, load_secret_key::rng_secret_key};
use reth_discv4::{DiscoveryUpdate, Discv4, Discv4Config};
use reth_discv5::{
    Config, Discv5,
    discv5::{ConfigBuilder as Discv5ConfigBuilder, Event, ListenConfig, ProtocolIdentity},
};
use reth_net_nat::{NatResolver, external_addr_with};
use reth_network_peers::NodeRecord;
use secp256k1::SecretKey;
use tokio::{net::UdpSocket, select};
use tokio_stream::StreamExt;
use tracing::{info, warn};

/// Start a discovery-only bootnode.
#[derive(Parser, Clone, Debug)]
pub struct Command {
    /// UDP listen address for discv4, shared with discv5 when `--v5` is set.
    ///
    /// Uses a distinct clap `id` so it doesn't collide with `base`'s global `--metrics.addr`,
    /// whose id is `addr`.
    #[arg(id = "bootnode_addr", long = "addr", alias = "v4-addr", default_value = "0.0.0.0:30301")]
    pub addr: SocketAddr,

    /// Secret key for the bootnode. Deterministically sets the peer ID.
    /// If the path exists, the key is loaded; otherwise a new key is generated and saved there.
    /// If omitted, an ephemeral key is used.
    #[arg(long, value_name = "PATH")]
    pub p2p_secret_key: Option<PathBuf>,

    /// NAT resolution method (any|none|upnp|publicip|extip:\<IP\>)
    #[arg(long, default_value = "any")]
    pub nat: NatResolver,

    /// Also run discv5, sharing the discv4 UDP port (`--addr`).
    #[arg(long)]
    pub v5: bool,
}

impl Command {
    /// Execute the bootnode command.
    pub async fn execute(self) -> eyre::Result<()> {
        info!(addr = %self.addr, nat = %self.nat, v5 = self.v5, "Bootnode starting");

        let sk = self.network_secret()?;
        let node_record = NodeRecord::from_secret_key(self.addr, &sk);
        let discv4_config = self.discv4_config();

        // With `--v5`, discv5 owns the shared socket's read loop and hands discv4 packets back as
        // unrecognized frames, which are forwarded to the discv4 ingress below.
        let (_discv4, mut discv4_service, _discv5, mut discv5_events) = if self.v5 {
            let socket = Arc::new(UdpSocket::bind(self.addr).await?);
            let (discv4, discv4_service, discv4_ingress) =
                Discv4::bind_shared(Arc::clone(&socket), None, node_record, sk, discv4_config)?;

            // Resolve dynamic NAT methods (`any`, `upnp`, `publicip`) too: discv5 only learns
            // its IP from peers after they can reach it, so a bootnode ENR must carry one up front.
            let external_ip = external_addr_with(self.nat.clone()).await;
            if external_ip.is_none() {
                warn!(
                    addr = %self.addr,
                    "Could not resolve external IP via NAT; discv5 ENR has no IP and may not be reachable"
                );
            }
            let (discv5, updates) =
                Discv5::start(&sk, self.discv5_config(socket, external_ip)).await?;
            info!(enr = %discv5.local_enr(), "Started discv5");

            (discv4, discv4_service, Some(discv5), Some((updates, discv4_ingress)))
        } else {
            let (discv4, discv4_service) =
                Discv4::bind(self.addr, node_record, sk, discv4_config).await?;
            (discv4, discv4_service, None, None)
        };
        info!(node_record = ?node_record, enode = %node_record, "Started discv4");
        let mut discv4_updates = discv4_service.update_stream();
        discv4_service.spawn();

        loop {
            select! {
                update = discv4_updates.next() => {
                    match update {
                        Some(DiscoveryUpdate::Added(record)) => {
                            info!(peer_id = ?record.id, "discv4 peer added");
                        }
                        Some(DiscoveryUpdate::Removed(peer_id)) => {
                            info!(peer_id = ?peer_id, "discv4 peer removed");
                        }
                        Some(_) => {}
                        None => {
                            info!("discv4 update stream ended");
                            break;
                        }
                    }
                }
                update = async {
                    if let Some((updates, _)) = &mut discv5_events {
                        updates.recv().await
                    } else {
                        futures::future::pending().await
                    }
                } => {
                    match update {
                        Some(Event::UnrecognizedFrame(frame)) => {
                            if let Some((_, discv4_ingress)) = &mut discv5_events {
                                discv4_ingress.handle_packet(&frame.packet, frame.src_address).await;
                            }
                        }
                        Some(Event::SessionEstablished(enr, _)) => {
                            info!(peer_id = ?enr.id(), "discv5 session established");
                        }
                        Some(_) => {}
                        None => {
                            info!("discv5 update stream ended");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Build the discv4 configuration with NAT-based external IP resolution.
    pub fn discv4_config(&self) -> Discv4Config {
        Discv4Config::builder().external_ip_resolver(Some(self.nat.clone())).build()
    }

    /// Build the discv5 configuration serving discv5 on the shared discv4 `socket`, advertising
    /// `external_ip` in the local ENR when known.
    pub fn discv5_config(&self, socket: Arc<UdpSocket>, external_ip: Option<IpAddr>) -> Config {
        let listen_config = if self.addr.is_ipv4() {
            ListenConfig::FromSockets { ipv4: Some(socket), ipv6: None }
        } else {
            ListenConfig::FromSockets { ipv4: None, ipv6: Some(socket) }
        };
        let mut inner_builder = Discv5ConfigBuilder::new(listen_config.clone());

        inner_builder.protocol_identity(ProtocolIdentity {
            protocol_id: BASE_V0_PROTOCOL_VERSION,
            ..Default::default()
        });

        let builder = Config::builder(self.addr).discv5_config(inner_builder.build());
        let mut config = match external_ip {
            Some(ip) => builder.advertised_ip(ip).build(),
            None => builder.build(),
        };
        // `build` rewrites the listen config from socket addresses, which would drop the shared
        // socket, so restore it afterwards.
        config.discv5_config_mut().listen_config = listen_config;
        config
    }

    fn network_secret(&self) -> eyre::Result<SecretKey> {
        match &self.p2p_secret_key {
            Some(path) => Ok(get_secret_key(path)?),
            None => Ok(rng_secret_key()),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn cmd(addr: &str) -> Command {
        Command {
            addr: addr.parse().unwrap(),
            p2p_secret_key: None,
            nat: NatResolver::None,
            v5: false,
        }
    }

    #[rstest]
    #[case(NatResolver::None)]
    #[case(NatResolver::ExternalIp("192.0.2.1".parse().unwrap()))]
    fn discv4_config_matches_refactored_builder(#[case] nat: NatResolver) {
        let mut command = cmd("0.0.0.0:30301");
        command.nat = nat;

        let actual = command.discv4_config();
        let expected =
            Discv4Config::builder().external_ip_resolver(Some(command.nat.clone())).build();

        assert_eq!(actual.udp_egress_message_buffer, expected.udp_egress_message_buffer);
        assert_eq!(actual.udp_ingress_message_buffer, expected.udp_ingress_message_buffer);
        assert_eq!(actual.max_find_node_failures, expected.max_find_node_failures);
        assert_eq!(actual.ping_interval, expected.ping_interval);
        assert_eq!(actual.ping_expiration, expected.ping_expiration);
        assert_eq!(actual.lookup_interval, expected.lookup_interval);
        assert_eq!(actual.request_timeout, expected.request_timeout);
        assert_eq!(actual.enr_expiration, expected.enr_expiration);
        assert_eq!(actual.neighbours_expiration, expected.neighbours_expiration);
        assert_eq!(actual.bootstrap_nodes, expected.bootstrap_nodes);
        assert_eq!(actual.enable_dht_random_walk, expected.enable_dht_random_walk);
        assert_eq!(actual.enable_lookup, expected.enable_lookup);
        assert_eq!(actual.enable_eip868, expected.enable_eip868);
        assert_eq!(actual.enforce_expiration_timestamps, expected.enforce_expiration_timestamps);
        assert_eq!(actual.additional_eip868_rlp_pairs, expected.additional_eip868_rlp_pairs);
        assert_eq!(actual.external_ip_resolver, expected.external_ip_resolver);
        assert_eq!(actual.resolve_external_ip_interval, expected.resolve_external_ip_interval);
        assert_eq!(actual.bond_expiration, expected.bond_expiration);

        assert_eq!(actual.external_ip_resolver, Some(command.nat));
    }

    #[test]
    fn v4_addr_alias_sets_shared_addr() {
        let command = Command::parse_from(["bootnode", "--v4-addr", "127.0.0.1:30305"]);

        assert_eq!(command.addr, "127.0.0.1:30305".parse().unwrap());
    }

    #[tokio::test]
    async fn discv5_shares_discv4_socket_and_advertises_external_ip() {
        let command = cmd("127.0.0.1:0");
        let socket = Arc::new(UdpSocket::bind(command.addr).await.unwrap());
        let shared_port = socket.local_addr().unwrap().port();
        let mut config =
            command.discv5_config(Arc::clone(&socket), Some("192.0.2.1".parse().unwrap()));
        assert_eq!(
            config.discv5_config_mut().protocol_identity.protocol_id,
            BASE_V0_PROTOCOL_VERSION
        );

        // discv5 must start on the port the shared socket already holds instead of rebinding it.
        let (discv5, _updates) =
            Discv5::start(&SecretKey::from_byte_array(&[1u8; 32]).unwrap(), config).await.unwrap();

        let enr = discv5.local_enr();
        assert_eq!(enr.ip4(), Some("192.0.2.1".parse().unwrap()));
        assert_eq!(enr.udp4(), Some(shared_port));
    }

    #[tokio::test]
    async fn v5_bootnode_answers_discv4_on_shared_port() {
        let key_dir = tempfile::tempdir().unwrap();
        let key_path = key_dir.path().join("bootnode.key");
        let bootnode_id = NodeRecord::from_secret_key(
            "127.0.0.1:0".parse().unwrap(),
            &get_secret_key(&key_path).unwrap(),
        )
        .id;
        let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let bootnode_addr = SocketAddr::from(([127, 0, 0, 1], port));
        let bootnode = tokio::spawn(
            Command {
                addr: bootnode_addr,
                p2p_secret_key: Some(key_path),
                v5: true,
                ..cmd("127.0.0.1:0")
            }
            .execute(),
        );

        let client_sk = rng_secret_key();
        let client_record = NodeRecord::from_secret_key("127.0.0.1:0".parse().unwrap(), &client_sk);
        let client_config = Discv4Config::builder()
            .add_boot_node(NodeRecord::new(bootnode_addr, bootnode_id))
            .build();
        let (_client, mut client_service) =
            Discv4::bind("127.0.0.1:0".parse().unwrap(), client_record, client_sk, client_config)
                .await
                .unwrap();
        let mut client_updates = client_service.update_stream();
        let _client_service = client_service.spawn();

        let bonded = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(update) = client_updates.next().await {
                if matches!(update, DiscoveryUpdate::Added(record) if record.id == bootnode_id) {
                    return true;
                }
            }
            false
        })
        .await;

        bootnode.abort();
        assert_eq!(bonded, Ok(true));
    }
}
