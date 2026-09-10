//! Canonical chain state notification trait and types.

use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use alloy_eips::BlockNumHash;
use base_common_types_chain::SealedHeader;
use base_execution_state_types::CanonStateNotification;
use derive_more::{Deref, DerefMut};
use tokio::sync::{broadcast, watch};
use tokio_stream::{
    Stream,
    wrappers::{BroadcastStream, WatchStream},
};
use tracing::debug;

/// Type alias for a receiver that receives [`CanonStateNotification`]
pub type CanonStateNotifications = broadcast::Receiver<CanonStateNotification>;

/// Type alias for a sender that sends [`CanonStateNotification`]
pub type CanonStateNotificationSender = broadcast::Sender<CanonStateNotification>;

/// A type that allows to register chain related event subscriptions.
pub trait CanonStateSubscriptions: Send + Sync {
    /// Get notified when a new canonical chain was imported.
    ///
    /// A canonical chain be one or more blocks, a reorg or a revert.
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications;

    /// Convenience method to get a stream of [`CanonStateNotification`].
    fn canonical_state_stream(&self) -> CanonStateNotificationStream {
        CanonStateNotificationStream {
            st: BroadcastStream::new(self.subscribe_to_canonical_state()),
        }
    }
}

impl<T: CanonStateSubscriptions> CanonStateSubscriptions for &T {
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications {
        (*self).subscribe_to_canonical_state()
    }

    fn canonical_state_stream(&self) -> CanonStateNotificationStream {
        (*self).canonical_state_stream()
    }
}

/// A Stream of [`CanonStateNotification`].
#[derive(Debug)]
#[pin_project::pin_project]
pub struct CanonStateNotificationStream {
    #[pin]
    st: BroadcastStream<CanonStateNotification>,
}

impl CanonStateNotificationStream {
    /// Wraps a canonical-state subscription as a stream, skipping lag notifications.
    pub fn new(receiver: CanonStateNotifications) -> Self {
        Self { st: BroadcastStream::new(receiver) }
    }
}

impl Stream for CanonStateNotificationStream {
    type Item = CanonStateNotification;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match ready!(self.as_mut().project().st.poll_next(cx)) {
                Some(Ok(notification)) => Poll::Ready(Some(notification)),
                Some(Err(err)) => {
                    debug!(%err, "canonical state notification stream lagging behind");
                    continue;
                }
                None => Poll::Ready(None),
            };
        }
    }
}

/// Wrapper around a broadcast receiver that receives fork choice notifications.
#[derive(Debug, Deref, DerefMut)]
pub struct ForkChoiceNotifications(pub watch::Receiver<Option<SealedHeader>>);

/// A trait that allows to register to fork choice related events
/// and get notified when a new fork choice is available.
pub trait ForkChoiceSubscriptions: Send + Sync {
    /// Get notified when a new safe block of the chain is selected.
    fn subscribe_safe_block(&self) -> ForkChoiceNotifications;

    /// Get notified when a new finalized block of the chain is selected.
    fn subscribe_finalized_block(&self) -> ForkChoiceNotifications;

    /// Convenience method to get a stream of the new safe blocks of the chain.
    fn safe_block_stream(&self) -> ForkChoiceStream<SealedHeader> {
        ForkChoiceStream::new(self.subscribe_safe_block().0)
    }

    /// Convenience method to get a stream of the new finalized blocks of the chain.
    fn finalized_block_stream(&self) -> ForkChoiceStream<SealedHeader> {
        ForkChoiceStream::new(self.subscribe_finalized_block().0)
    }
}

/// A stream that yields values from a `watch::Receiver<Option<T>>`, filtering out `None` values.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct WatchValueStream<T> {
    #[pin]
    st: WatchStream<Option<T>>,
}

impl<T: Clone + Sync + Send + 'static> WatchValueStream<T> {
    /// Creates a new [`WatchValueStream`]
    pub fn new(rx: watch::Receiver<Option<T>>) -> Self {
        Self { st: WatchStream::from_changes(rx) }
    }
}

impl<T: Clone + Sync + Send + 'static> Stream for WatchValueStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(self.as_mut().project().st.poll_next(cx)) {
                Some(Some(notification)) => return Poll::Ready(Some(notification)),
                Some(None) => {}
                None => return Poll::Ready(None),
            }
        }
    }
}

/// Alias for [`WatchValueStream`] for fork choice watch channels.
pub type ForkChoiceStream<T> = WatchValueStream<T>;

/// Wrapper around a watch receiver that receives persisted block notifications.
#[derive(Debug, Deref, DerefMut)]
pub struct PersistedBlockNotifications(pub watch::Receiver<Option<BlockNumHash>>);

/// A trait that allows subscribing to persisted block events.
pub trait PersistedBlockSubscriptions: Send + Sync {
    /// Get notified when a new block is persisted to disk.
    fn subscribe_persisted_block(&self) -> PersistedBlockNotifications;

    /// Convenience method to get a stream of the persisted blocks.
    fn persisted_block_stream(&self) -> WatchValueStream<BlockNumHash> {
        WatchValueStream::new(self.subscribe_persisted_block().0)
    }
}
