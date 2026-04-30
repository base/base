//! Bootnode command with discv5 NAT fix.

use std::{net::SocketAddr, path::PathBuf};

use base_node_core::BASE_V0_PROTOCOL_VERSION;
use clap::Parser;
use reth_cli_util::{get_secret_key, load_secret_key::rng_secret_key};
use reth_discv4::{DiscoveryUpdate, Discv4, Discv4Config};
use reth_discv5::{
    Config, DEFAULT_DISCOVERY_V5_LISTEN_CONFIG, Discv5,
    discv5::{ConfigBuilder as Discv5ConfigBuilder, Event, ProtocolIdentity},
};
use reth_net_nat::{NatResolver, external_addr_with};
use reth_network_peers::NodeRecord;
use secp256k1::SecretKey;
use tokio::select;
use tokio_stream::StreamExt;
use tracing::{info, warn};

/// Start a discovery-only bootnode.
#[derive(Parser, Debug)]
pub struct Command {
    /// Listen address for the bootnode for discv4
    #[arg(long, default_value = "0.0.0.0:30301")]
    pub v4_addr: SocketAddr,

    /// Listen address for the bootnode for discv5
    #[arg(long, default_value = "0.0.0.0:9200")]
    pub v5_addr: SocketAddr,

    /// Secret key for the bootnode. Deterministically sets the peer ID.
    /// If the path exists, the key is loaded; otherwise a new key is generated and saved there.
    /// If omitted, an ephemeral key is used.
    #[arg(long, value_name = "PATH")]
    pub p2p_secret_key: Option<PathBuf>,

    /// NAT resolution method (any|none|upnp|publicip|extip:\<IP\>)
    #[arg(long, default_value = "any")]
    pub nat: NatResolver,

    /// Run a discv5 topic discovery bootnode in addition to discv4.
    #[arg(long)]
    pub v5: bool,

    /// Enable the Base discv5 protocol identity.
    #[arg(long = "v5.base-protocol", default_value_t = true, action = clap::ArgAction::Set)]
    pub base_protocol: bool,
}

impl Command {
    /// Execute the bootnode command.
    pub async fn execute(self) -> eyre::Result<()> {
        info!(v4_addr = %self.v4_addr, v5_addr = %self.v5_addr, nat = %self.nat, v5 = self.v5, "Bootnode starting");

        // discv4
        let sk = self.network_secret()?;
        let v4_node_record = NodeRecord::from_secret_key(self.v4_addr, &sk);
        let config = self.discv4_config();
        let nat = self.nat.clone();
        let (_discv4, mut discv4_service) =
            Discv4::bind(self.v4_addr, v4_node_record, sk, config).await?;
        info!(v4_node_record = ?v4_node_record, enode = %v4_node_record, "Started discv4");
        let mut discv4_updates = discv4_service.update_stream();
        discv4_service.spawn();

        // discv5
        let mut discv5_updates = None;
        let mut _discv5 = None;

        if self.v5 {
            info!("Initializing discv5");
            let config = self.discv5_config();
            let (discv5, updates) = Discv5::start(&sk, config).await?;

            // The upstream reth bootnode skips NAT resolution for discv5, leaving the ENR with
            // no IP address. Peers receiving the ENR cannot send WHOAREYOU back because they
            // have no address to target. Resolve the external IP and update the ENR here.
            match external_addr_with(nat).await {
                Some(external_ip) => {
                    let socket = SocketAddr::new(external_ip, self.v5_addr.port());
                    discv5.with_discv5(|d| d.update_local_enr_socket(socket, false));
                }
                None => {
                    warn!(
                        addr = %self.v5_addr,
                        "Could not resolve external IP via NAT; discv5 ENR has no IP and may not be reachable"
                    );
                }
            }

            info!(enr = %discv5.local_enr(), "Started discv5");

            discv5_updates = Some(updates);
            _discv5 = Some(discv5);
        }

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
                    if let Some(updates) = &mut discv5_updates {
                        updates.recv().await
                    } else {
                        futures::future::pending().await
                    }
                } => {
                    match update {
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

    /// Build the discv5 configuration.
    pub fn discv5_config(&self) -> Config {
        let mut inner_builder = Discv5ConfigBuilder::new(DEFAULT_DISCOVERY_V5_LISTEN_CONFIG);

        if self.base_protocol {
            inner_builder.protocol_identity(ProtocolIdentity {
                protocol_id: BASE_V0_PROTOCOL_VERSION,
                ..Default::default()
            });
        }

        Config::builder(self.v5_addr).discv5_config(inner_builder.build()).build()
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
    use reth_discv5::DEFAULT_DISCOVERY_V5_PORT;
    use rstest::rstest;

    use super::*;

    fn cmd(v4_addr: &str, v5_addr: &str) -> Command {
        Command {
            v4_addr: v4_addr.parse().unwrap(),
            v5_addr: v5_addr.parse().unwrap(),
            p2p_secret_key: None,
            nat: NatResolver::None,
            v5: false,
            base_protocol: true,
        }
    }

    #[rstest]
    #[case(NatResolver::None)]
    #[case(NatResolver::ExternalIp("192.0.2.1".parse().unwrap()))]
    fn discv4_config_matches_refactored_builder(#[case] nat: NatResolver) {
        let mut command = cmd("0.0.0.0:30301", "0.0.0.0:9200");
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

    #[rstest]
    #[case("0.0.0.0:30301", "0.0.0.0:9200")]
    #[case("0.0.0.0:30303", "0.0.0.0:9000")]
    #[case("127.0.0.1:10001", "127.0.0.1:10002")]
    fn discv5_config_preserves_default_discovery_socket_and_sets_rlpx_socket(
        #[case] v4_addr: &str,
        #[case] v5_addr: &str,
    ) {
        let command = cmd(v4_addr, v5_addr);
        let config = command.discv5_config();

        assert_eq!(config.rlpx_socket(), &command.v5_addr);
        assert_eq!(config.discovery_socket().ip(), command.v5_addr.ip());
        assert_eq!(config.discovery_socket().port(), DEFAULT_DISCOVERY_V5_PORT);
    }
}
