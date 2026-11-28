//! Configuration of the tp2p gossip transport.

use std::{
    fmt,
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
    str::FromStr,
};

use alloy_primitives::{B256, hex};
use secp256k1_tp2p::PublicKey;

/// Which gossip transports carry unsafe payloads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tp2pMode {
    /// libp2p only, as without tp2p.
    #[default]
    Off,
    /// libp2p and tp2p side by side: payloads are published on both and taken from both.
    Both,
    /// tp2p only: neither libp2p nor discv5 is started.
    Only,
}

/// A [`Tp2pMode`] that is not `off`, `both` or `only`.
#[derive(Debug, thiserror::Error)]
#[error("invalid tp2p mode {0:?}, expected off, both or only")]
pub struct Tp2pModeParseError(pub String);

impl FromStr for Tp2pMode {
    type Err = Tp2pModeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "both" => Ok(Self::Both),
            "only" => Ok(Self::Only),
            other => Err(Tp2pModeParseError(other.to_string())),
        }
    }
}

impl fmt::Display for Tp2pMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Both => "both",
            Self::Only => "only",
        })
    }
}

/// A tp2p peer: its static key and address, written `tp2p://<compressed pubkey hex>@host:port`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tp2pPeer {
    /// The peer's static secp256k1 key, which the Noise XK handshake expects.
    pub key: PublicKey,
    /// Where to dial it.
    pub addr: SocketAddr,
}

impl Tp2pPeer {
    /// The URL scheme of a peer address.
    pub const SCHEME: &'static str = "tp2p://";
}

/// Why a [`Tp2pPeer`] failed to parse.
#[derive(Debug, thiserror::Error)]
pub enum Tp2pPeerParseError {
    /// Not `tp2p://<key>@<host:port>`.
    #[error("expected tp2p://<66 hex digit compressed public key>@host:port, got {0:?}")]
    Format(String),
    /// The key is not a 33-byte compressed secp256k1 public key.
    #[error("invalid tp2p public key {0:?}")]
    Key(String),
    /// The address did not resolve.
    #[error("invalid tp2p address {0:?}")]
    Address(String),
}

impl FromStr for Tp2pPeer {
    type Err = Tp2pPeerParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (key, addr) = s
            .trim()
            .strip_prefix(Self::SCHEME)
            .and_then(|rest| rest.split_once('@'))
            .ok_or_else(|| Tp2pPeerParseError::Format(s.to_string()))?;
        let key = hex::decode(key)
            .ok()
            .filter(|bytes| bytes.len() == 33)
            .and_then(|bytes| PublicKey::from_slice(&bytes).ok())
            .ok_or_else(|| Tp2pPeerParseError::Key(key.to_string()))?;
        let addr = addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .ok_or_else(|| Tp2pPeerParseError::Address(addr.to_string()))?;
        Ok(Self { key, addr })
    }
}

impl fmt::Display for Tp2pPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}@{}", Self::SCHEME, hex::encode(self.key.serialize()), self.addr)
    }
}

/// Configuration of the tp2p gossip transport.
#[derive(derive_more::Debug, Clone, PartialEq, Eq)]
pub struct Tp2pConfig {
    /// Which transports run.
    pub mode: Tp2pMode,
    /// The TCP address to listen on.
    pub listen: SocketAddr,
    /// The address advertised in the node's signed record (default: the listen address).
    pub advertise: Option<SocketAddr>,
    /// Peers dialed first and redialed whenever disconnected.
    pub bootnodes: Vec<Tp2pPeer>,
    /// Peers never penalized, banned or refused for capacity; they are also dialed like
    /// bootnodes.
    pub trusted: Vec<Tp2pPeer>,
    /// The node's static secp256k1 secret key.
    #[debug(skip)]
    pub secret: B256,
    /// Where tp2p keeps its anchors and tried peers across restarts; `None` keeps nothing.
    pub peers_file: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_round_trips_and_rejects_malformed_input() {
        // The compressed secp256k1 generator point.
        let key = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let raw = format!("tp2p://{key}@127.0.0.1:9224");
        let peer: Tp2pPeer = raw.parse().unwrap();
        assert_eq!(peer.addr, "127.0.0.1:9224".parse().unwrap());
        assert_eq!(peer.to_string(), raw);

        for bad in [
            format!("{key}@127.0.0.1:9224"),
            format!("tp2p://{key}"),
            format!("tp2p://04{}@127.0.0.1:9224", "11".repeat(64)),
            format!("tp2p://{}@127.0.0.1:9224", "zz".repeat(33)),
            format!("tp2p://{key}@127.0.0.1"),
        ] {
            assert!(bad.parse::<Tp2pPeer>().is_err(), "{bad}");
        }
    }
}
