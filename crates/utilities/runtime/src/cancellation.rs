//! Cancellation signal abstraction replacing `tokio_util::sync::CancellationToken`.

use std::{future::Future, pin::Pin};

/// A shareable cancellation signal.
///
/// Replaces `tokio_util::sync::CancellationToken`. The primary usage pattern —
/// `runtime.cancelled()` as a `tokio::select!` arm — is preserved exactly:
///
/// ```rust,ignore
/// tokio::select! {
///     biased;
///     _ = self.runtime.cancelled() => { return Ok(()); }
///     event = self.source.next() => { /* ... */ }
/// }
/// ```
///
/// # Cancel safety
///
/// The future returned by [`cancelled`](Cancellation::cancelled) is
/// cancel-safe: dropping and recreating it does not miss a cancellation that
/// was signalled while the future was not being polled. Each call to
/// `cancelled()` checks the current state immediately on first poll.
pub trait Cancellation: Clone + Send + Sync + 'static {
    /// Returns a cancel-safe future that resolves when cancellation is signalled.
    ///
    /// If cancellation was already signalled before this future is polled, it
    /// resolves immediately on the first poll.
    ///
    /// The returned future is `'static`: it does not borrow `self`.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Signal cancellation. All futures from [`cancelled`](Cancellation::cancelled)
    /// will resolve on their next poll.
    ///
    /// Idempotent: calling `cancel` multiple times is safe.
    fn cancel(&self);

    /// Non-blocking check: has cancellation been signalled?
    fn is_cancelled(&self) -> bool;

    /// Returns a child handle that is cancelled when the parent is cancelled,
    /// but can also be cancelled independently without affecting the parent.
    fn child(&self) -> Self;
}
