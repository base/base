//! Block subscription trait for keepalive-aware streaming.

use base_alloy_consensus::OpBlock;
use futures::stream::BoxStream;

use crate::SourceError;

/// A source of an unsafe-block stream that may hold ancillary resources.
///
/// Unlike a plain [`Stream`], an implementor can own resources (e.g. a WebSocket
/// provider handle) that must remain alive for the duration of the stream. The
/// stream itself is obtained once by calling [`take_stream`]; the implementor is
/// then held by [`HybridBlockSource`] to keep those resources live.
///
/// [`Stream`]: futures::Stream
/// [`take_stream`]: BlockSubscription::take_stream
/// [`HybridBlockSource`]: crate::HybridBlockSource
pub trait BlockSubscription: Send {
    /// Extract the block stream from this subscription.
    ///
    /// Must be called at most once; implementors may panic on a second call.
    fn take_stream(&mut self) -> BoxStream<'static, Result<OpBlock, SourceError>>;
}
