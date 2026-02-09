//! Contains the [`BaseNodeHandle`], an awaitable handle to a launched Base node.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use derive_more::Debug;
use eyre::Result;
use futures_util::{FutureExt, future::BoxFuture};
use reth_node_builder::NodeHandleFor;

use crate::node::BaseNode;

/// Handle to a launched Base node.
///
/// This wraps the underlying [`NodeHandleFor`] so callers can await it directly to wait for node
/// shutdown.
#[must_use = "Dropping the handle will stop the node immediately"]
#[derive(Debug, Default)]
pub struct BaseNodeHandle {
    #[debug(skip)]
    build_fut: Option<BoxFuture<'static, Result<NodeHandleFor<BaseNode>>>>,
    #[debug(skip)]
    handle: Option<Box<NodeHandleFor<BaseNode>>>,
}

impl BaseNodeHandle {
    pub(crate) fn new(
        fut: impl Future<Output = Result<NodeHandleFor<BaseNode>>> + Send + 'static,
    ) -> Self {
        Self { build_fut: Some(fut.boxed()), handle: None }
    }
}

impl Future for BaseNodeHandle {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(build_fut) = self.build_fut.as_mut() {
            match build_fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(handle)) => {
                    self.handle = Some(Box::new(handle));
                    self.build_fut = None;
                }
                Poll::Ready(Err(err)) => {
                    self.build_fut = None;
                    return Poll::Ready(Err(err));
                }
            }
        }

        if let Some(handle) = self.handle.as_mut() {
            // SAFETY: The handle is stored on the heap so its address is stable; we never move the
            // inner value after taking this reference.
            let node_exit_future = unsafe { Pin::new_unchecked(&mut handle.node_exit_future) };
            let poll = node_exit_future.poll(cx);
            if poll.is_ready() {
                self.handle = None;
            }
            return poll;
        }

        // If neither future is present we completed successfully and have nothing left to drive.
        Poll::Ready(Ok(()))
    }
}
