//! Native implementation of the [Channel] trait, backed by [`async_channel`]'s unbounded
//! channel primitives.

use std::io::Result;

use async_channel::{Receiver, Sender, unbounded};
use async_trait::async_trait;

use crate::{
    Channel,
    errors::{ChannelError, ChannelResult},
};

/// A bidirectional channel, allowing for synchronized communication between two parties.
#[derive(Debug, Clone)]
pub struct BidirectionalChannel {
    /// The client handle of the channel.
    pub client: NativeChannel,
    /// The host handle of the channel.
    pub host: NativeChannel,
}

impl BidirectionalChannel {
    /// Creates a [`BidirectionalChannel`] instance.
    pub fn new() -> Result<Self> {
        let (bw, ar) = unbounded();
        let (aw, br) = unbounded();

        Ok(Self {
            client: NativeChannel { read: ar, write: aw },
            host: NativeChannel { read: br, write: bw },
        })
    }
}

/// A channel with a receiver and sender.
///
/// Each read consumes one complete message. [`Channel::read`] returns an error without
/// modifying the buffer if the message is too large, and [`Channel::read_exact`] also
/// returns an error if the message is shorter than the buffer; the message is discarded.
#[derive(Debug, Clone)]
pub struct NativeChannel {
    /// The receiver of the channel.
    pub(crate) read: Receiver<Vec<u8>>,
    /// The sender of the channel.
    pub(crate) write: Sender<Vec<u8>>,
}

#[async_trait]
impl Channel for NativeChannel {
    async fn read(&self, buf: &mut [u8]) -> ChannelResult<usize> {
        let data = self.read.recv().await.map_err(|_| ChannelError::Closed)?;
        if data.len() > buf.len() {
            return Err(ChannelError::BufferTooSmall {
                message_len: data.len(),
                buffer_len: buf.len(),
            });
        }
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    async fn read_exact(&self, buf: &mut [u8]) -> ChannelResult<usize> {
        let len = self.read(buf).await?;
        if len != buf.len() {
            return Err(ChannelError::UnexpectedEOF);
        }
        Ok(len)
    }

    async fn write(&self, buf: &[u8]) -> ChannelResult<usize> {
        self.write.send(buf.to_vec()).await.map_err(|_| ChannelError::Closed)?;
        Ok(buf.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_rejects_oversized_message() {
        let channel = BidirectionalChannel::new().unwrap();
        channel.host.write(b"too long").await.unwrap();
        let mut buffer = [0; 3];

        assert!(matches!(
            channel.client.read(&mut buffer).await,
            Err(ChannelError::BufferTooSmall { message_len: 8, buffer_len: 3 })
        ));
        assert_eq!(buffer, [0; 3]);
    }

    #[tokio::test]
    async fn read_exact_rejects_short_message() {
        let channel = BidirectionalChannel::new().unwrap();
        channel.host.write(b"ab").await.unwrap();
        let mut buffer = [0; 3];

        assert!(matches!(
            channel.client.read_exact(&mut buffer).await,
            Err(ChannelError::UnexpectedEOF)
        ));
    }
}
