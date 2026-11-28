//! tp2p gossip transport CLI flags.

use std::{net::SocketAddr, path::PathBuf};

use alloy_primitives::B256;
use base_consensus_node::{Tp2pConfig, Tp2pMode, Tp2pPeer};
use clap::Args;
use libp2p::identity::Keypair;

/// tp2p gossip transport flags: tp2p carries unsafe payloads alongside libp2p or instead of it.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Tp2pArgs {
    /// Which gossip transports carry unsafe payloads: `off` (libp2p only), `both` (libp2p and
    /// tp2p; payloads are published on both and taken from both), or `only` (tp2p only; neither
    /// libp2p nor discv5 is started).
    #[arg(
        id = "tp2p_mode",
        value_name = "MODE",
        long = "p2p.tp2p",
        default_value_t = Tp2pMode::Off,
        env = "BASE_NODE_P2P_TP2P"
    )]
    pub mode: Tp2pMode,

    /// TCP address tp2p listens on.
    #[arg(
        id = "tp2p_listen",
        value_name = "IP:PORT",
        long = "p2p.tp2p.listen",
        default_value = "0.0.0.0:9224",
        env = "BASE_NODE_P2P_TP2P_LISTEN"
    )]
    pub listen: SocketAddr,

    /// Address advertised to tp2p peers in the node's signed record. Defaults to the listen
    /// address, which peers cannot dial when it is unspecified (`0.0.0.0`).
    #[arg(
        id = "tp2p_advertise",
        value_name = "IP:PORT",
        long = "p2p.tp2p.advertise",
        env = "BASE_NODE_P2P_TP2P_ADVERTISE"
    )]
    pub advertise: Option<SocketAddr>,

    /// Comma-separated tp2p bootnodes, as `tp2p://<66 hex digit compressed public key>@host:port`.
    /// They are dialed first and redialed whenever disconnected.
    #[arg(
        id = "tp2p_bootnodes",
        value_name = "PEERS",
        long = "p2p.tp2p.bootnodes",
        value_delimiter = ',',
        env = "BASE_NODE_P2P_TP2P_BOOTNODES"
    )]
    pub bootnodes: Vec<Tp2pPeer>,

    /// Comma-separated trusted tp2p peers, in the bootnode format. They are never penalized,
    /// banned or refused for capacity, and are dialed like bootnodes.
    #[arg(
        id = "tp2p_trusted",
        value_name = "PEERS",
        long = "p2p.tp2p.trusted",
        value_delimiter = ',',
        env = "BASE_NODE_P2P_TP2P_TRUSTED"
    )]
    pub trusted: Vec<Tp2pPeer>,

    /// File holding the hex-encoded 32-byte secp256k1 secret key of the node's tp2p identity,
    /// created if it does not exist. Defaults to the libp2p key (`--p2p.priv.path` or
    /// `--p2p.priv.raw`), so the tp2p public key equals the libp2p peer's secp256k1 key.
    #[arg(
        id = "tp2p_priv_path",
        value_name = "PATH",
        long = "p2p.tp2p.priv-path",
        env = "BASE_NODE_P2P_TP2P_PRIV_PATH"
    )]
    pub priv_path: Option<PathBuf>,

    /// File where tp2p keeps its anchors (the outbound peers it dials first after a restart) and
    /// the peers it connected to, so a restart keeps its outbound slots with peers that served it
    /// instead of starting from bootnodes and peer exchange alone. Unset keeps nothing.
    #[arg(
        id = "tp2p_peers_path",
        value_name = "PATH",
        long = "p2p.tp2p.peers-path",
        env = "BASE_NODE_P2P_TP2P_PEERS_PATH"
    )]
    pub peers_path: Option<PathBuf>,
}

impl Tp2pArgs {
    /// Returns the tp2p configuration, or none when tp2p is off. The key comes from
    /// `--p2p.tp2p.priv-path`, else from `libp2p_keypair`.
    pub fn config(&self, libp2p_keypair: &Keypair) -> eyre::Result<Option<Tp2pConfig>> {
        if self.mode == Tp2pMode::Off {
            return Ok(None);
        }
        let keypair = match &self.priv_path {
            Some(path) => base_consensus_peers::SecretKeyLoader::load(path)
                .map_err(|e| eyre::eyre!("Failed to load tp2p key from {}: {e}", path.display()))?,
            None => libp2p_keypair.clone(),
        };
        let secret = keypair
            .try_into_secp256k1()
            .map_err(|e| eyre::eyre!("tp2p needs a secp256k1 key: {e}"))?
            .secret()
            .to_bytes();
        Ok(Some(Tp2pConfig {
            mode: self.mode,
            listen: self.listen,
            advertise: self.advertise,
            bootnodes: self.bootnodes.clone(),
            trusted: self.trusted.clone(),
            secret: B256::from(secret),
            peers_file: self.peers_path.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser, Debug)]
    struct Command {
        #[command(flatten)]
        tp2p: Tp2pArgs,
    }

    #[test]
    fn defaults_to_off_and_reuses_the_libp2p_key() {
        let keypair = Keypair::generate_secp256k1();
        let args = Command::parse_from(["test"]).tp2p;
        assert_eq!(args.mode, Tp2pMode::Off);
        assert_eq!(args.listen, "0.0.0.0:9224".parse().unwrap());
        assert!(args.config(&keypair).unwrap().is_none());

        let args = Command::parse_from(["test", "--p2p.tp2p", "both"]).tp2p;
        let config = args.config(&keypair).unwrap().unwrap();
        let expected = keypair.try_into_secp256k1().unwrap().secret().to_bytes();
        assert_eq!(config.mode, Tp2pMode::Both);
        assert_eq!(config.secret, B256::from(expected));
    }

    #[test]
    fn parses_peers_and_rejects_unknown_modes() {
        let peer = "tp2p://0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798@127.0.0.1:9224";
        let args = Command::parse_from([
            "test",
            "--p2p.tp2p",
            "only",
            "--p2p.tp2p.bootnodes",
            &format!("{peer},{peer}"),
            "--p2p.tp2p.trusted",
            peer,
        ])
        .tp2p;
        assert_eq!(args.mode, Tp2pMode::Only);
        assert_eq!(args.bootnodes.len(), 2);
        assert_eq!(args.trusted[0].to_string(), peer);

        assert!(Command::try_parse_from(["test", "--p2p.tp2p", "on"]).is_err());
        assert!(Command::try_parse_from(["test", "--p2p.tp2p.bootnodes", "enr:x"]).is_err());
    }
}
