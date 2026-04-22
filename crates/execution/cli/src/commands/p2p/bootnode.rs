//! Bootnode command with discv5 NAT fix.

use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use reth_cli_util::{get_secret_key, load_secret_key::rng_secret_key};
use reth_discv4::{DiscoveryUpdate, Discv4, Discv4Config};
use reth_discv5::{Config, Discv5, discv5::Event};
use reth_net_nat::{NatResolver, external_addr_with};
use reth_network_peers::NodeRecord;
use secp256k1::SecretKey;
use tokio::select;
use tokio_stream::StreamExt;
use tracing::{info, warn};

/// Start a discovery-only bootnode.
#[derive(Parser, Debug)]
pub struct Command {
    /// Listen address for the bootnode.
    #[arg(long, default_value = "0.0.0.0:30301")]
    pub addr: SocketAddr,

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
}

impl Command {
    /// Execute the bootnode command.
    pub async fn execute(self) -> eyre::Result<()> {
        info!(addr = %self.addr, nat = %self.nat, v5 = self.v5, "Bootnode starting");

        let sk = self.network_secret()?;
        let local_enr = NodeRecord::from_secret_key(self.addr, &sk);

        let nat = self.nat;
        let config = Discv4Config::builder().external_ip_resolver(Some(nat.clone())).build();
        let (_discv4, mut discv4_service) = Discv4::bind(self.addr, local_enr, sk, config).await?;

        info!(enr = ?local_enr, "Started discv4");

        let mut discv4_updates = discv4_service.update_stream();
        discv4_service.spawn();

        let mut discv5_updates = None;
        let mut _discv5 = None;

        if self.v5 {
            let config = Config::builder(self.addr).build();
            let (discv5, updates) = Discv5::start(&sk, config).await?;

            // The upstream reth bootnode skips NAT resolution for discv5, leaving the ENR with
            // no IP address. Peers receiving the ENR cannot send WHOAREYOU back because they
            // have no address to target. Resolve the external IP and update the ENR here.
            match external_addr_with(nat).await {
                Some(external_ip) => {
                    let udp_socket = SocketAddr::new(external_ip, self.addr.port());
                    discv5.with_discv5(|d| d.update_local_enr_socket(udp_socket, false));
                    info!(enr = %discv5.local_enr(), "Started discv5");
                }
                None => {
                    warn!(
                        addr = %self.addr,
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

    fn network_secret(&self) -> eyre::Result<SecretKey> {
        match &self.p2p_secret_key {
            Some(path) => Ok(get_secret_key(path)?),
            None => Ok(rng_secret_key()),
        }
    }
}
