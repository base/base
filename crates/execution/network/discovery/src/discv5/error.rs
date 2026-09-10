//! Errors interfacing with [`crate::Discv5Protocol`].

/// Errors interfacing with [`crate::Discv5Protocol`].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Failure adding node to [`crate::Discv5Protocol`].
    #[error("failed adding node to discv5, {0}")]
    AddNodeFailed(&'static str),
    /// Node record has incompatible key type.
    #[error("incompatible key type (not secp256k1)")]
    IncompatibleKeyType,
    /// No key used to identify rlpx network is configured.
    #[error("network stack identifier is not configured")]
    NetworkStackIdNotConfigured,
    /// Missing key used to identify rlpx network.
    #[error("fork missing on enr, key {0:?} and key 'eth' missing")]
    ForkMissing(&'static [u8]),
    /// Failed to decode [`ForkId`](alloy_eip2124::ForkId) rlp value.
    #[error("failed to decode fork id, 'eth': {0:?}")]
    ForkIdDecodeError(#[from] alloy_rlp::Error),
    /// Peer is unreachable over discovery.
    #[error("discovery socket missing")]
    UnreachableDiscovery,
    /// Failed to initialize [`crate::Discv5Protocol`].
    #[error("init failed, {0}")]
    InitFailure(&'static str),
    /// An error from underlying [`crate::Discv5Protocol`] node.
    #[error("sigp/discv5 error, {0}")]
    Discv5Error(crate::Error),
    /// The [`ListenConfig`](crate::ListenConfig) has been misconfigured.
    #[error("misconfigured listen config, RLPx TCP address must also be supported by discv5")]
    ListenConfigMisconfigured,
}
