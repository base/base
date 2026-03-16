//! L1 head subscription trait for keepalive-aware streaming.

use futures::stream::BoxStream;

use crate::SourceError;

/// A source of an L1 head number stream that may hold ancillary resources.
///
/// Unlike a plain [`Stream`], an implementor can own resources (e.g. a WebSocket
/// provider handle) that must remain alive for the duration of the stream. The
/// stream itself is obtained once by calling [`take_stream`]; the implementor is
/// then held by [`HybridL1HeadSource`] to keep those resources live.
///
/// [`Stream`]: futures::Stream
/// [`take_stream`]: L1HeadSubscription::take_stream
/// [`HybridL1HeadSource`]: crate::HybridL1HeadSource
pub trait L1HeadSubscription: Send {
    /// Extract the L1 head number stream from this subscription.
    ///
    /// Must be called at most once; implementors may panic on a second call.
    fn take_stream(&mut self) -> BoxStream<'static, Result<u64, SourceError>>;
}
