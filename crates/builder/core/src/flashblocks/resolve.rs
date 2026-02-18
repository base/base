use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_util::FutureExt;
use reth_payload_builder::PayloadBuilderError;

use crate::flashblocks::cell::WaitForValue;

/// A future that resolves when a payload becomes available in the [`BlockCell`].
pub struct ResolvePayload<T> {
    pub future: WaitForValue<T>,
}

impl<T> std::fmt::Debug for ResolvePayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvePayload").finish_non_exhaustive()
    }
}

impl<T> ResolvePayload<T> {
    pub const fn new(future: WaitForValue<T>) -> Self {
        Self { future }
    }
}

impl<T: Clone> Future for ResolvePayload<T> {
    type Output = Result<T, PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut().future.poll_unpin(cx) {
            Poll::Ready(value) => Poll::Ready(Ok(value)),
            Poll::Pending => Poll::Pending,
        }
    }
}
