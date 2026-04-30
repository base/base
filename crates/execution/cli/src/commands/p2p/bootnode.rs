//! Bootnode command with discv5 NAT fix.

use std::{net::SocketAddr, path::PathBuf};

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
use tokio::select;
use tokio_stream::StreamExt;
use tracing::{info, warn};

/// Start a discovery-only bootnode.
#[derive(Parser, Debug)]
pub struct Command {
    /// Listen address for discv4.
    #[arg(long, default_value = "0.0.0.0:30301")]
    pub v4_addr: SocketAddr,

    /// UDP listen address for discv5 (only used with --v5).
    /// Must differ from --v4-addr since discv5 binds its own socket exclusively.
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
        info!(v4_addr = %self.v4_addr, v5_addr = %self.v5_addr, nat = %self.nat, v5 = %self.v5, "Bootnode starting");

        let v4_addr = self.v4_addr;
        let v5_addr = self.v5_addr;
        let sk = self.network_secret()?;
        let local_enr = NodeRecord::from_secret_key(v4_addr, &sk);
        let discv4_config = self.discv4_config();
        let discv5_config = self.v5.then(|| self.discv5_config());
        let nat = self.nat;
        let (_discv4, mut discv4_service) =
            Discv4::bind(v4_addr, local_enr, sk, discv4_config).await?;
        info!(enr = ?local_enr, "Started discv4");
        let mut discv4_updates = discv4_service.update_stream();
        discv4_service.spawn();

        let mut discv5_updates = None;
        let mut _discv5 = None;

        if let Some(discv5_config) = discv5_config {
            let (discv5, updates) = Discv5::start(&sk, discv5_config).await?;

            // The upstream reth bootnode skips NAT resolution for discv5, leaving the ENR with
            // no IP address. Peers receiving the ENR cannot send WHOAREYOU back because they
            // have no address to target. Resolve the external IP and update the ENR here.
            match external_addr_with(nat).await {
                Some(external_ip) => {
                    let udp_socket = SocketAddr::new(external_ip, v5_addr.port());
                    discv5.with_discv5(|d| d.update_local_enr_socket(udp_socket, false));
                    info!(enr = %discv5.local_enr(), "Started discv5");
                }
                None => {
                    warn!(
                        v5_addr = %v5_addr,
                        "Could not resolve external IP via NAT; discv5 ENR has no IP and may not be reachable"
                    );
                    info!(enr = %discv5.local_enr(), "Started discv5");
                }
            }

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

    /// Build the discv5 configuration with the UDP listen port set from `--v5-addr`.
    pub fn discv5_config(&self) -> Config {
        let listen = ListenConfig::from_ip(self.v5_addr.ip(), self.v5_addr.port());
        let mut inner_builder = Discv5ConfigBuilder::new(listen);

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
    #[case("0.0.0.0:30301", "0.0.0.0:9200")]
    #[case("0.0.0.0:30303", "0.0.0.0:9000")]
    #[case("127.0.0.1:10001", "127.0.0.1:10002")]
    fn discv5_discovery_port_matches_v5_addr(#[case] v4_addr: &str, #[case] v5_addr: &str) {
        let command = cmd(v4_addr, v5_addr);
        let discv5_port = command.discv5_config().discovery_socket().port();

        assert_eq!(discv5_port, command.v5_addr.port());
        assert_ne!(discv5_port, command.v4_addr.port());
    }

    #[test]
    fn discv4_config_builds() {
        let _config = cmd("0.0.0.0:30301", "0.0.0.0:9200").discv4_config();
    }

    #[test]
    fn discv5_config_builds() {
        let _config = cmd("0.0.0.0:30301", "0.0.0.0:9200").discv5_config();
    }
}
