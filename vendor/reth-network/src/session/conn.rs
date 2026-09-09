//! Connection types for a session

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Sink, SinkExt, Stream, StreamExt};
use reth_ecies::stream::ECIESStream;
use reth_eth_wire::{
    EthMessage, EthSnapMessage, EthSnapStream, EthStream, EthVersion, P2PStream,
    errors::{EthStreamError, P2PStreamError},
    message::EthBroadcastMessage,
    snap::SnapProtocolMessage,
};
use reth_eth_wire_types::RawCapabilityMessage;
use tokio::net::TcpStream;

/// The type of the underlying peer network connection.
pub type EthPeerConnection = EthStream<P2PStream<ECIESStream<TcpStream>>>;

/// A dedicated `eth` + `snap/2` connection.
pub type EthSnapConnection = EthSnapStream<ECIESStream<TcpStream>>;

/// Native ETH or ETH/SNAP connection used by a Base peer.
// This type is boxed because the underlying stream is ~6KB,
// mostly coming from `P2PStream`'s `snap::Encoder` (2072), and `ECIESStream` (3600).
#[derive(Debug)]
pub enum EthRlpxConnection {
    /// A connection that only supports the ETH protocol.
    EthOnly(Box<EthPeerConnection>),
    /// A dedicated connection that supports the ETH protocol and `snap/2` (EIP-8189).
    EthSnap(Box<EthSnapConnection>),
}

impl EthRlpxConnection {
    /// Returns the negotiated ETH version.
    #[inline]
    pub(crate) const fn version(&self) -> EthVersion {
        match self {
            Self::EthOnly(conn) => conn.version(),
            Self::EthSnap(conn) => conn.version(),
        }
    }

    /// Returns `true` if `snap/2` was negotiated on this connection.
    #[inline]
    pub(crate) const fn supports_snap(&self) -> bool {
        matches!(self, Self::EthSnap(_))
    }

    /// Consumes this type and returns the wrapped [`P2PStream`].
    #[inline]
    pub(crate) fn into_inner(self) -> P2PStream<ECIESStream<TcpStream>> {
        match self {
            Self::EthOnly(conn) => conn.into_inner(),
            Self::EthSnap(conn) => conn.into_inner(),
        }
    }

    /// Returns mutable access to the underlying stream.
    #[inline]
    pub(crate) fn inner_mut(&mut self) -> &mut P2PStream<ECIESStream<TcpStream>> {
        match self {
            Self::EthOnly(conn) => conn.inner_mut(),
            Self::EthSnap(conn) => conn.inner_mut(),
        }
    }

    /// Returns access to the underlying stream.
    #[inline]
    pub(crate) const fn inner(&self) -> &P2PStream<ECIESStream<TcpStream>> {
        match self {
            Self::EthOnly(conn) => conn.inner(),
            Self::EthSnap(conn) => conn.inner(),
        }
    }

    /// Same as [`Sink::start_send`] but accepts a [`EthBroadcastMessage`] instead.
    #[inline]
    pub fn start_send_broadcast(
        &mut self,
        item: EthBroadcastMessage,
    ) -> Result<(), EthStreamError> {
        match self {
            Self::EthOnly(conn) => conn.start_send_broadcast(item),
            Self::EthSnap(conn) => conn.start_send_broadcast(item),
        }
    }

    /// Sends a raw capability message over the connection
    pub fn start_send_raw(&mut self, msg: RawCapabilityMessage) -> Result<(), EthStreamError> {
        match self {
            Self::EthOnly(conn) => conn.start_send_raw(msg),
            Self::EthSnap(conn) => conn.start_send_raw(msg),
        }
    }

    /// Queues a `snap/2` message to be sent on the wire.
    ///
    /// Returns an error on connections that did not negotiate `snap/2`, so a caller never believes
    /// a request was sent when it was discarded.
    pub fn start_send_snap(&mut self, msg: SnapProtocolMessage) -> Result<(), EthStreamError> {
        match self {
            Self::EthSnap(conn) => conn.start_send_unpin(EthSnapMessage::Snap(msg)),
            Self::EthOnly(_) => Err(P2PStreamError::CapabilityNotShared.into()),
        }
    }

    /// Sets whether to reject block announcement messages (`NewBlock`, `NewBlockHashes`) before
    /// RLP decoding to avoid memory amplification from deserializing blocks that will be discarded.
    pub fn set_reject_block_announcements(&mut self, reject: bool) {
        match self {
            Self::EthOnly(conn) => conn.set_reject_block_announcements(reject),
            Self::EthSnap(conn) => conn.set_reject_block_announcements(reject),
        }
    }
}

impl From<EthPeerConnection> for EthRlpxConnection {
    #[inline]
    fn from(conn: EthPeerConnection) -> Self {
        Self::EthOnly(Box::new(conn))
    }
}

impl From<EthSnapConnection> for EthRlpxConnection {
    #[inline]
    fn from(conn: EthSnapConnection) -> Self {
        Self::EthSnap(Box::new(conn))
    }
}

/// Delegates a call to the active variant's boxed stream (every variant is `Unpin`).
///
/// The second form runs `$adapt` on the eth-only variants to lift their result into the shared
/// item type; the snap variant already yields it.
macro_rules! delegate_call {
    ($self:ident.$method:ident($($args:ident),+)) => {
        match $self.get_mut() {
            Self::EthOnly(l) => l.$method($($args),+),
            Self::EthSnap(s) => s.$method($($args),+),
        }
    };
    ($self:ident.$method:ident($($args:ident),+) => $adapt:expr) => {
        match $self.get_mut() {
            Self::EthOnly(l) => $adapt(l.$method($($args),+)),
            Self::EthSnap(s) => s.$method($($args),+),
        }
    };
}

impl Stream for EthRlpxConnection {
    type Item = Result<EthSnapMessage, EthStreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        delegate_call!(self.poll_next_unpin(cx) => lift_eth)
    }
}

impl Sink<EthMessage> for EthRlpxConnection {
    type Error = EthStreamError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        delegate_call!(self.poll_ready_unpin(cx))
    }

    fn start_send(self: Pin<&mut Self>, item: EthMessage) -> Result<(), Self::Error> {
        match self.get_mut() {
            Self::EthOnly(l) => l.start_send_unpin(item),
            Self::EthSnap(s) => s.start_send_unpin(EthSnapMessage::Eth(item)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        delegate_call!(self.poll_flush_unpin(cx))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        delegate_call!(self.poll_close_unpin(cx))
    }
}

/// Lifts a polled `eth` item into the shared [`EthSnapMessage`] item type.
#[inline]
fn lift_eth(
    poll: Poll<Option<Result<EthMessage, EthStreamError>>>,
) -> Poll<Option<Result<EthSnapMessage, EthStreamError>>> {
    poll.map(|opt| opt.map(|res| res.map(EthSnapMessage::Eth)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn assert_eth_stream<St>()
    where
        St: Stream<Item = Result<EthMessage, EthStreamError>> + Sink<EthMessage>,
    {
    }

    const fn assert_eth_snap_stream<St>()
    where
        St: Stream<Item = Result<EthSnapMessage, EthStreamError>> + Sink<EthMessage>,
    {
    }

    #[test]
    const fn test_eth_stream_variants() {
        assert_eth_stream::<EthPeerConnection>();
        assert_eth_snap_stream::<EthRlpxConnection>();
    }
}
