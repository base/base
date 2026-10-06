//! Channel-backed [`UnsafeBlockSource`] for tests.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{L2BlockEvent, UnsafeBlockSource};

/// An [`UnsafeBlockSource`] backed by a `tokio::sync::mpsc` unbounded channel.
///
/// Use [`ChannelBlockSource::new`] to obtain a `(source, sender)` pair.
/// Events sent on the [`mpsc::UnboundedSender`] side are consumed by
/// [`UnsafeBlockSource::next`]. Once all senders are dropped, `next` parks forever.
#[derive(Debug)]
pub struct ChannelBlockSource {
    rx: mpsc::UnboundedReceiver<L2BlockEvent>,
}

impl ChannelBlockSource {
    /// Create a new channel block source and its corresponding sender handle.
    pub fn new() -> (Self, mpsc::UnboundedSender<L2BlockEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx }, tx)
    }
}

#[async_trait]
impl UnsafeBlockSource for ChannelBlockSource {
    async fn next(&mut self) -> L2BlockEvent {
        match self.rx.recv().await {
            Some(event) => event,
            None => std::future::pending().await,
        }
    }
}
