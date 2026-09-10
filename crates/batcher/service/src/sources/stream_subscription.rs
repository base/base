//! Generic stream subscription wrappers.

use std::{any::Any, sync::Arc};

use futures::{StreamExt, stream::BoxStream};

use crate::SourceError;

/// A subscription backed directly by a stream.
#[derive(derive_more::Debug)]
pub struct StreamSubscription<T> {
    #[debug("{:?}", stream.as_ref().map(|_| "<stream>"))]
    stream: Option<BoxStream<'static, Result<T, SourceError>>>,
}

impl<T> StreamSubscription<T> {
    /// Creates a subscription from a stream.
    pub fn new(stream: BoxStream<'static, Result<T, SourceError>>) -> Self {
        Self { stream: Some(stream) }
    }

    /// Extracts the underlying stream.
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    pub fn take_stream(&mut self) -> BoxStream<'static, Result<T, SourceError>> {
        self.stream.take().expect("take_stream called more than once")
    }
}

/// A [`StreamSubscription`] that keeps an ancillary resource alive.
///
/// This is used for WebSocket subscriptions where the provider connection must
/// outlive the stream handed to a hybrid source.
#[derive(derive_more::Debug)]
pub struct KeepAliveSubscription<T> {
    #[debug(skip)]
    _resource: Arc<dyn Any + Send + Sync>,
    inner: StreamSubscription<T>,
}

impl<T> KeepAliveSubscription<T> {
    /// Creates a subscription from a resource and stream.
    pub fn new<P: Any + Send + Sync + 'static>(
        resource: Arc<P>,
        stream: BoxStream<'static, Result<T, SourceError>>,
    ) -> Self {
        Self { _resource: resource, inner: StreamSubscription::new(stream) }
    }

    /// Extracts the underlying stream.
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    pub fn take_stream(&mut self) -> BoxStream<'static, Result<T, SourceError>> {
        self.inner.take_stream()
    }
}

/// A subscription that never yields items.
#[derive(Debug)]
pub struct PendingSubscription<T> {
    _marker: std::marker::PhantomData<T>,
}

impl<T> PendingSubscription<T> {
    /// Creates a pending subscription.
    pub const fn new() -> Self {
        Self { _marker: std::marker::PhantomData }
    }
}

impl<T> PendingSubscription<T>
where
    T: Send + 'static,
{
    /// Returns a stream that never yields.
    pub fn take_stream(&mut self) -> BoxStream<'static, Result<T, SourceError>> {
        futures::stream::pending().boxed()
    }
}

impl<T> Default for PendingSubscription<T> {
    fn default() -> Self {
        Self::new()
    }
}
