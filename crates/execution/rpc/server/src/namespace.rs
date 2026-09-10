//! Built-in RPC namespaces served by Base.

use serde::{Deserialize, Serialize};

/// RPC namespaces installed on every enabled Base transport.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Hash,
    Deserialize,
    Serialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum RpcNamespace {
    /// The `admin` namespace.
    Admin,
    /// The `debug` namespace.
    Debug,
    /// The `eth` namespace.
    Eth,
    /// The `net` namespace.
    Net,
    /// The `trace` namespace.
    Trace,
    /// The `txpool` namespace.
    Txpool,
    /// The `web3` namespace.
    Web3,
    /// The `rpc` namespace.
    Rpc,
    /// The `reth` namespace.
    Reth,
    /// The `ots` namespace.
    Ots,
    /// The `miner` namespace.
    Miner,
    /// The `mev` namespace.
    Mev,
}

impl RpcNamespace {
    /// All built-in namespaces, in registration order.
    pub const ALL: &'static [Self] = &[
        Self::Admin,
        Self::Debug,
        Self::Eth,
        Self::Net,
        Self::Trace,
        Self::Txpool,
        Self::Web3,
        Self::Rpc,
        Self::Reth,
        Self::Ots,
        Self::Miner,
        Self::Mev,
    ];

    /// Returns the built-in namespaces.
    pub fn modules() -> impl Iterator<Item = Self> + Clone {
        Self::ALL.iter().copied()
    }

    /// Returns the namespace name.
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}
