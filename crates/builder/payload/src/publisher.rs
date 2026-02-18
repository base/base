/// Trait for publishing flashblock payloads to subscribers.
use std::{fmt, sync::Arc};

use base_primitives::FlashblocksPayloadV1;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// Trait for publishing flashblock payloads to subscribers.
///
/// Implement this trait to define how flashblock payloads are broadcast
/// (e.g., via WebSocket, in-memory channel for testing, etc.).
pub trait FlashblockPublisher: Send + Sync {
    /// Publish a flashblock payload. Returns the byte size of the published data.
    fn publish(&self, payload: &FlashblocksPayloadV1) -> eyre::Result<usize>;
}

/// A no-op publisher that discards payloads. Useful for testing.
#[derive(Debug, Clone, Default)]
pub struct NoopFlashblockPublisher;

impl FlashblockPublisher for NoopFlashblockPublisher {
    fn publish(&self, _payload: &FlashblocksPayloadV1) -> eyre::Result<usize> {
        Ok(0)
    }
}

/// Result of a [`GuardedPublisher::publish_if_not_cancelled`] call.
#[derive(Debug)]
pub enum PublishResult {
    /// Published successfully, returning the byte size of the serialized payload.
    Published(usize),
    /// Skipped because the block cancellation token was already set when the lock was acquired.
    Cancelled,
}

/// Wraps a [`FlashblockPublisher`] with a mutex guard and cancellation check.
///
/// Ensures that the cancellation check and the publish are atomic: the caller
/// acquires the guard, verifies the token is still active, and publishes — or
/// skips if already cancelled. This prevents a race between `get_payload`
/// (resolve) cancellation and flashblock publishing.
pub struct GuardedPublisher {
    publisher: Arc<dyn FlashblockPublisher>,
    guard: Arc<Mutex<()>>,
}

impl fmt::Debug for GuardedPublisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardedPublisher").finish_non_exhaustive()
    }
}

impl GuardedPublisher {
    /// Creates a new [`GuardedPublisher`].
    pub fn new(publisher: Arc<dyn FlashblockPublisher>, guard: Arc<Mutex<()>>) -> Self {
        Self { publisher, guard }
    }

    /// Atomically check cancellation and publish the payload.
    ///
    /// Acquires the publish guard, checks the cancellation token, and publishes
    /// only if the token is not cancelled.
    pub fn publish_if_not_cancelled(
        &self,
        payload: &FlashblocksPayloadV1,
        cancel: &CancellationToken,
    ) -> eyre::Result<PublishResult> {
        let _guard = self.guard.lock();
        if cancel.is_cancelled() {
            return Ok(PublishResult::Cancelled);
        }
        let size = self.publisher.publish(payload)?;
        Ok(PublishResult::Published(size))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_primitives::FlashblocksPayloadV1;
    use parking_lot::Mutex;
    use tokio_util::sync::CancellationToken;

    use super::{GuardedPublisher, NoopFlashblockPublisher, PublishResult};

    fn test_payload() -> FlashblocksPayloadV1 {
        FlashblocksPayloadV1::default()
    }

    fn guarded(cancel: &CancellationToken) -> GuardedPublisher {
        GuardedPublisher::new(Arc::new(NoopFlashblockPublisher), Arc::new(Mutex::new(())))
    }

    #[test]
    fn test_publishes_when_not_cancelled() {
        let cancel = CancellationToken::new();
        let g = guarded(&cancel);
        let result = g.publish_if_not_cancelled(&test_payload(), &cancel).unwrap();
        assert!(matches!(result, PublishResult::Published(0)));
    }

    #[test]
    fn test_skips_when_already_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let g = guarded(&cancel);
        let result = g.publish_if_not_cancelled(&test_payload(), &cancel).unwrap();
        assert!(matches!(result, PublishResult::Cancelled));
    }

    #[test]
    fn test_shared_guard_serializes_publishes() {
        let cancel = CancellationToken::new();
        let guard = Arc::new(Mutex::new(()));
        let g1 = GuardedPublisher::new(Arc::new(NoopFlashblockPublisher), Arc::clone(&guard));
        let g2 = GuardedPublisher::new(Arc::new(NoopFlashblockPublisher), Arc::clone(&guard));
        // Both should succeed; the shared guard ensures they serialize.
        let r1 = g1.publish_if_not_cancelled(&test_payload(), &cancel).unwrap();
        let r2 = g2.publish_if_not_cancelled(&test_payload(), &cancel).unwrap();
        assert!(matches!(r1, PublishResult::Published(0)));
        assert!(matches!(r2, PublishResult::Published(0)));
    }
}
